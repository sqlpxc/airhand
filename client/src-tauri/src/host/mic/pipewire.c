// Publish a client-owned PipeWire source for this session. Closing our connection (including
// process death) removes the node. No server module, saved WirePlumber rule or default change.
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <time.h>
#include <pipewire/pipewire.h>
#include <spa/param/audio/format-utils.h>
#include <spa/utils/result.h>
#include "pcm.h"

struct source {
    AHPCM pcm;
    struct pw_main_loop *main;
    struct pw_context *context;
    struct pw_core *core;
    struct spa_hook core_listener, stream_listener;
    struct pw_stream *stream;
    int connection_error;
};

static pthread_once_t init_once = PTHREAD_ONCE_INIT;
static void init_pipewire(void) { pw_init(NULL, NULL); }

static void on_error(void *data, uint32_t id, int seq, int res, const char *message) {
    (void)seq;
    (void)message;
    if (id == PW_ID_CORE) ((struct source *)data)->connection_error = res;
}
static const struct pw_core_events core_events = {.version = PW_VERSION_CORE_EVENTS, .error = on_error};

static void process(void *data) {
    struct source *s = data;
    struct pw_buffer *buffer = pw_stream_dequeue_buffer(s->stream);
    if (!buffer) return;
    struct spa_data *d = &buffer->buffer->datas[0];
    if (d->data && d->chunk) {
        const uint32_t capacity = d->maxsize / sizeof(float);
        uint32_t frames = buffer->requested ? SPA_MIN(buffer->requested, capacity) : capacity;
        ah_read(&s->pcm, d->data, frames);
        d->chunk->offset = 0;
        d->chunk->stride = sizeof(float);
        d->chunk->size = frames * sizeof(float);
        buffer->size = frames;
    }
    pw_stream_queue_buffer(s->stream, buffer);
}
static const struct pw_stream_events stream_events = {.version = PW_VERSION_STREAM_EVENTS, .process = process};

static void close_source(struct source *s) {
    if (s->stream) pw_stream_destroy(s->stream);
    if (s->core) { spa_hook_remove(&s->core_listener); pw_core_disconnect(s->core); }
    if (s->context) pw_context_destroy(s->context);
    if (s->main) {
        pw_loop_leave(pw_main_loop_get_loop(s->main));
        pw_main_loop_destroy(s->main);
    }
    free(s);
}

static struct source *connect_source(char *error, size_t size) {
    pthread_once(&init_once, init_pipewire);
    struct source *s = calloc(1, sizeof(*s));
    if (s && (s->main = pw_main_loop_new(NULL))) {
        struct pw_loop *loop = pw_main_loop_get_loop(s->main);
        pw_loop_enter(loop);
        s->context = pw_context_new(loop, NULL, 0);
        if (s->context && (s->core = pw_context_connect(s->context, NULL, 0))) {
            pw_core_add_listener(s->core, &s->core_listener, &core_events, s);
            return s;
        }
    }
    snprintf(error, size, "connect to the user's PipeWire server: %s", strerror(errno));
    if (s) close_source(s);
    return NULL;
}

int ah_mic_probe(char *error, size_t size) {
    struct source *s = connect_source(error, size);
    if (!s) return -1;
    close_source(s);
    return 0;
}

static int poll_source(struct source *s, int timeout, char *error, size_t size) {
    int res = pw_loop_iterate(pw_main_loop_get_loop(s->main), timeout);
    if (s->connection_error) res = s->connection_error;
    if (res < 0 && res != -EINTR) {
        snprintf(error, size, "PipeWire connection: %s", spa_strerror(res));
        return -1;
    }
    const char *reason = NULL;
    enum pw_stream_state state = pw_stream_get_state(s->stream, &reason);
    if (state == PW_STREAM_STATE_ERROR || state == PW_STREAM_STATE_UNCONNECTED) {
        snprintf(error, size, "PipeWire source: %s", reason ? reason : "disconnected");
        return -1;
    }
    return state == PW_STREAM_STATE_PAUSED || state == PW_STREAM_STATE_STREAMING;
}

static int64_t now_ms(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

void *ah_mic_open(const char *uid, const char *unused, const char *name, char *error, size_t size) {
    (void)unused;
    struct source *s = connect_source(error, size);
    if (!s) return NULL;
    struct pw_properties *props = pw_properties_new(
        PW_KEY_NODE_NAME, uid, PW_KEY_NODE_DESCRIPTION, name,
        PW_KEY_MEDIA_CLASS, "Audio/Source", PW_KEY_NODE_VIRTUAL, "true",
        PW_KEY_NODE_ALWAYS_PROCESS, "true", PW_KEY_PRIORITY_SESSION, "1",
        PW_KEY_OBJECT_LINGER, "false", NULL);
    if (props) s->stream = pw_stream_new(s->core, name, props);
    if (s->stream) {
        pw_stream_add_listener(s->stream, &s->stream_listener, &stream_events, s);
        uint8_t storage[512];
        struct spa_pod_builder builder = SPA_POD_BUILDER_INIT(storage, sizeof(storage));
        const struct spa_pod *format = spa_format_audio_raw_build(&builder, SPA_PARAM_EnumFormat,
            &SPA_AUDIO_INFO_RAW_INIT(.format = SPA_AUDIO_FORMAT_F32_LE, .rate = AH_PCM_RATE,
                                     .channels = 1, .position = { SPA_AUDIO_CHANNEL_MONO }));
        int res = pw_stream_connect(s->stream, PW_DIRECTION_OUTPUT, PW_ID_ANY,
            PW_STREAM_FLAG_MAP_BUFFERS | PW_STREAM_FLAG_RT_PROCESS, &format, 1);
        if (res >= 0) {
            const int64_t deadline = now_ms() + 2000;
            while (now_ms() < deadline) {
                int ready = poll_source(s, 20, error, size);
                if (ready > 0) return s;
                if (ready < 0) { close_source(s); return NULL; }
            }
            res = -ETIMEDOUT;
        }
        snprintf(error, size, "publish Airhand Microphone: %s", spa_strerror(res));
    } else {
        snprintf(error, size, "create PipeWire source: %s", strerror(errno));
    }
    close_source(s);
    return NULL;
}

int ah_mic_queued(void *handle, char *error, size_t size) {
    struct source *s = handle;
    if (poll_source(s, 0, error, size) < 0) return -1;
    return (int)ah_queued(&s->pcm);
}

void ah_mic_write(void *handle, const uint8_t *bytes, uint32_t frames) {
    struct source *s = handle;
    ah_write(&s->pcm, bytes, frames);
}

int ah_mic_close(void *handle, char *error, size_t size) {
    (void)error;
    (void)size;
    close_source(handle);
    return 0;
}
