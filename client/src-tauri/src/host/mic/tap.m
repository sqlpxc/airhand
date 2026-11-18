// macOS 14.2+: expose only this host process's mono playout through a public Core Audio tap.
// The audio engine starts with an empty ring. Its output is muted by the tap before Rust can
// write any voice, and stopped before we remove the tap (also on partial setup failures).
#import <AVFAudio/AVFAudio.h>
#import <CoreAudio/AudioHardwareTapping.h>
#import <CoreAudio/CATapDescription.h>
#import <Foundation/Foundation.h>
#include <unistd.h>
#include "pcm.h"

@interface AHMicSamples : NSObject { @public AHPCM pcm; }
@end
@implementation AHMicSamples
@end

@interface AHMicOutput : NSObject {
@public
    AHMicSamples *samples;
    AVAudioEngine *engine;
    AudioObjectID tap, device;
}
@end
@implementation AHMicOutput
@end

static OSStatus translate(AudioObjectPropertySelector selector, UInt32 size, const void *key,
                          AudioObjectID *object) {
    AudioObjectPropertyAddress prop = {selector, kAudioObjectPropertyScopeGlobal,
                                       kAudioObjectPropertyElementMain};
    UInt32 bytes = sizeof(*object);
    *object = kAudioObjectUnknown;
    return AudioObjectGetPropertyData(kAudioObjectSystemObject, &prop, size, key, &bytes, object);
}

static int failure(char *error, size_t size, const char *operation, OSStatus status) {
    snprintf(error, size, "%s (Core Audio %d)", operation, (int)status);
    return -1;
}

// Look up only our two per-user identifiers. Try both even if one object no longer exists.
static int cleanup(const char *uid, const char *uuid, char *error, size_t size) API_AVAILABLE(macos(14.2)) {
    NSString *keys[] = {@(uid), @(uuid)};
    AudioObjectPropertySelector selectors[] = {kAudioHardwarePropertyTranslateUIDToDevice,
                                                kAudioHardwarePropertyTranslateUIDToTap};
    int result = 0;
    for (int i = 0; i < 2; ++i) {
        CFStringRef key = (__bridge CFStringRef)keys[i];
        AudioObjectID id;
        OSStatus status = translate(selectors[i], sizeof(key), &key, &id);
        if (!status && id != kAudioObjectUnknown) {
            status = i == 0 ? AudioHardwareDestroyAggregateDevice(id) : AudioHardwareDestroyProcessTap(id);
        }
        if (status) result = failure(error, size, i == 0 ? "remove Airhand Microphone" : "remove Airhand audio tap", status);
    }
    return result;
}

// The Rust entry points check objc2::available!(macos = 14.2) before entering these APIs.
int ah_mic_cleanup(const char *uid, const char *uuid, char *error, size_t size) API_AVAILABLE(macos(14.2)) {
    @autoreleasepool { return cleanup(uid, uuid, error, size); }
}

static OSStatus closeOutput(AHMicOutput *output) API_AVAILABLE(macos(14.2)) {
    [output->engine stop];
    OSStatus deviceStatus = output->device ? AudioHardwareDestroyAggregateDevice(output->device) : noErr;
    OSStatus tapStatus = output->tap ? AudioHardwareDestroyProcessTap(output->tap) : noErr;
    return deviceStatus ? deviceStatus : tapStatus;
}

static bool startEngine(AHMicOutput *output, char *error, size_t size) {
    AHMicSamples *samples = [AHMicSamples new];
    output->samples = samples;
    AVAudioFormat *format = [[AVAudioFormat alloc] initStandardFormatWithSampleRate:AH_PCM_RATE channels:1];
    AVAudioSourceNode *source = [[AVAudioSourceNode alloc] initWithFormat:format renderBlock:
        ^OSStatus(BOOL *silence, const AudioTimeStamp *time, AVAudioFrameCount count, AudioBufferList *buffers) {
            (void)time;
            ah_read(&samples->pcm, buffers->mBuffers[0].mData, count);
            *silence = NO;
            return noErr;
        }];
    output->engine = [AVAudioEngine new];
    [output->engine attachNode:source];
    [output->engine connect:source to:output->engine.mainMixerNode format:format];
    NSError *cause = nil;
    if (![output->engine startAndReturnError:&cause]) {
        snprintf(error, size, "start microphone audio engine (%s %ld)", cause.domain.UTF8String, (long)cause.code);
        return false;
    }
    return true;
}

static void *openOutput(const char *uid, const char *uuid, const char *name, char *error, size_t size) API_AVAILABLE(macos(14.2)) {
    if (cleanup(uid, uuid, error, size) < 0) return NULL;
    AHMicOutput *output = [AHMicOutput new];
    @try {
        if (!startEngine(output, error, size)) return NULL;
        pid_t pid = getpid();
        AudioObjectID process;
        OSStatus status = translate(kAudioHardwarePropertyTranslatePIDToProcessObject, sizeof(pid), &pid, &process);
        if (status || !process) {
            failure(error, size, "find the microphone playout process", status);
        } else {
            CATapDescription *desc = [[CATapDescription alloc] initMonoMixdownOfProcesses:@[@(process)]];
            desc.name = @(name);
            desc.UUID = [[NSUUID alloc] initWithUUIDString:@(uuid)];
            desc.privateTap = NO;
            desc.muteBehavior = CATapMuted;
            status = AudioHardwareCreateProcessTap(desc, &output->tap);
            if (status) {
                failure(error, size, "create microphone tap", status);
            } else {
                NSDictionary *settings = @{
                    @kAudioAggregateDeviceUIDKey: @(uid), @kAudioAggregateDeviceNameKey: @(name),
                    @kAudioAggregateDeviceIsPrivateKey: @0,
                    @kAudioAggregateDeviceTapListKey: @[@{@kAudioSubTapUIDKey: desc.UUID.UUIDString,
                                                          @kAudioSubTapDriftCompensationKey: @YES}]
                };
                status = AudioHardwareCreateAggregateDevice((__bridge CFDictionaryRef)settings, &output->device);
                if (!status) return (__bridge_retained void *)output;
                failure(error, size, "publish Airhand Microphone", status);
            }
        }
    } @catch (NSException *exception) {
        // AVAudioEngine may raise when an output device disappears during graph setup.
        snprintf(error, size, "configure microphone audio engine (%s)", exception.name.UTF8String);
    }
    closeOutput(output);
    return NULL;
}

void *ah_mic_open(const char *uid, const char *uuid, const char *name, char *error, size_t size) API_AVAILABLE(macos(14.2)) {
    @autoreleasepool { return openOutput(uid, uuid, name, error, size); }
}

int ah_mic_queued(void *handle, char *error, size_t size) {
    AHMicOutput *output = (__bridge AHMicOutput *)handle;
    if (!output->engine.running) {
        snprintf(error, size, "microphone audio engine stopped");
        return -1;
    }
    return (int)ah_queued(&output->samples->pcm);
}

void ah_mic_write(void *handle, const uint8_t *bytes, uint32_t frames) {
    AHMicOutput *output = (__bridge AHMicOutput *)handle;
    ah_write(&output->samples->pcm, bytes, frames);
}

int ah_mic_close(void *handle, char *error, size_t size) API_AVAILABLE(macos(14.2)) {
    @autoreleasepool {
        AHMicOutput *output = (__bridge_transfer AHMicOutput *)handle;
        OSStatus status = closeOutput(output);
        return status ? failure(error, size, "close Airhand Microphone", status) : 0;
    }
}
