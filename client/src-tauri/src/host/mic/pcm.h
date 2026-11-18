// Single producer (the Opus playout thread), single consumer (the system audio callback).
// The callback only copies samples and zero-fills underruns. Neither side waits on the other.
#pragma once
#include <stdatomic.h>
#include <stdint.h>
#include <string.h>

#define AH_PCM_FRAMES 4096u
#define AH_PCM_RATE 48000u
_Static_assert(ATOMIC_INT_LOCK_FREE == 2, "audio callbacks need lock-free cursors");

typedef struct {
    _Atomic uint32_t read, written;
    float samples[AH_PCM_FRAMES];
} AHPCM;

// Only the producer calls queued/write. Its snapshot of the available space is conservative.
static inline uint32_t ah_queued(AHPCM *pcm) {
    return atomic_load_explicit(&pcm->written, memory_order_relaxed) -
           atomic_load_explicit(&pcm->read, memory_order_acquire);
}

static inline uint32_t ah_min(uint32_t a, uint32_t b) { return a < b ? a : b; }

static inline void ah_write(AHPCM *pcm, const uint8_t *bytes, uint32_t frames) {
    frames = ah_min(frames, AH_PCM_FRAMES - ah_queued(pcm));
    uint32_t written = atomic_load_explicit(&pcm->written, memory_order_relaxed);
    uint32_t offset = written % AH_PCM_FRAMES;
    uint32_t first = ah_min(frames, AH_PCM_FRAMES - offset);
    memcpy(pcm->samples + offset, bytes, first * sizeof(float));
    memcpy(pcm->samples, bytes + first * sizeof(float), (frames - first) * sizeof(float));
    atomic_store_explicit(&pcm->written, written + frames, memory_order_release);
}

static inline void ah_read(AHPCM *pcm, float *out, uint32_t frames) {
    uint32_t read = atomic_load_explicit(&pcm->read, memory_order_relaxed);
    uint32_t available = atomic_load_explicit(&pcm->written, memory_order_acquire) - read;
    uint32_t take = ah_min(frames, available);
    uint32_t offset = read % AH_PCM_FRAMES;
    uint32_t first = ah_min(take, AH_PCM_FRAMES - offset);
    memcpy(out, pcm->samples + offset, first * sizeof(float));
    memcpy(out + first, pcm->samples, (take - first) * sizeof(float));
    memset(out + take, 0, (frames - take) * sizeof(float));
    atomic_store_explicit(&pcm->read, read + take, memory_order_release);
}
