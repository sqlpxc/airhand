// Independent Core Audio input for e2e_mic. Read only the test aggregate, never a default input.
import CoreAudio
import Foundation

func check(_ status: OSStatus, _ operation: String) {
    if status != noErr {
        fputs("\(operation): Core Audio \(status)\n", stderr)
        exit(1)
    }
}
func property(_ selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress(mSelector: selector, mScope: kAudioObjectPropertyScopeGlobal,
                               mElement: kAudioObjectPropertyElementMain)
}

func lookup(_ selector: AudioObjectPropertySelector, _ uid: String) -> AudioObjectID {
    var key = uid as CFString
    var address = property(selector)
    var object = AudioObjectID(kAudioObjectUnknown)
    var size = UInt32(MemoryLayout.size(ofValue: object))
    withUnsafePointer(to: &key) {
        check(AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address,
                                         UInt32(MemoryLayout<CFString>.size), $0, &size, &object), "find test object")
    }
    return object
}
let device = lookup(kAudioHardwarePropertyTranslateUIDToDevice, CommandLine.arguments[1])
if CommandLine.arguments.last == "--count" {
    let tap = lookup(kAudioHardwarePropertyTranslateUIDToTap, CommandLine.arguments[2])
    print([device, tap].filter { $0 != kAudioObjectUnknown }.count)
    exit(0)
}
guard device != kAudioObjectUnknown else { exit(1) }
var rate = 0.0
var rateKey = property(kAudioDevicePropertyNominalSampleRate)
var size = UInt32(MemoryLayout.size(ofValue: rate))
check(AudioObjectGetPropertyData(device, &rateKey, 0, nil, &size, &rate), "get test input sample rate")
if rate != 48_000 {
    rate = 48_000
    check(AudioObjectSetPropertyData(device, &rateKey, 0, nil, size, &rate), "set test input sample rate")
}
var proc: AudioDeviceIOProcID?
check(AudioDeviceCreateIOProcIDWithBlock(&proc, device, nil) { _, input, _, _, _ in
    let buffer = input.pointee.mBuffers
    guard buffer.mNumberChannels == 1, let data = buffer.mData else { exit(1) }
    var offset = 0
    while offset < Int(buffer.mDataByteSize) {
        let n = write(STDOUT_FILENO, data.advanced(by: offset), Int(buffer.mDataByteSize) - offset)
        if n < 0 && errno == EINTR { continue }
        if n <= 0 { exit(1) }
        offset += n
    }
}, "create test input callback")
check(AudioDeviceStart(device, proc), "start test input")
RunLoop.current.run()
