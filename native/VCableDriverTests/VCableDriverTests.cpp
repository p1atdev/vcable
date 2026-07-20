#include <CoreAudio/AudioHardware.h>
#include <CoreAudio/AudioServerPlugIn.h>
#include <CoreFoundation/CoreFoundation.h>
#include <dlfcn.h>

#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>
#include <string_view>

namespace {

constexpr AudioObjectPropertySelector kVCableControlProperty = 'vctl';

CFPropertyListRef gStoredValue = nullptr;
UInt32 gNotifications = 0;
UInt32 gConfigurationRequests = 0;
AudioServerPlugInDriverRef gDriver = nullptr;
AudioObjectID gPendingDevice = kAudioObjectUnknown;
UInt64 gPendingAction = 0;
void* gPendingChangeInfo = nullptr;

OSStatus PropertiesChanged(AudioServerPlugInHostRef, AudioObjectID, UInt32 count,
                           const AudioObjectPropertyAddress*) {
    gNotifications += count;
    return noErr;
}

OSStatus CopyFromStorage(AudioServerPlugInHostRef, CFStringRef, CFPropertyListRef* output) {
    if (output == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    *output = gStoredValue;
    if (*output != nullptr) {
        CFRetain(*output);
    }
    return noErr;
}

OSStatus WriteToStorage(AudioServerPlugInHostRef, CFStringRef, CFPropertyListRef value) {
    if (value == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    CFRetain(value);
    if (gStoredValue != nullptr) {
        CFRelease(gStoredValue);
    }
    gStoredValue = value;
    return noErr;
}

OSStatus DeleteFromStorage(AudioServerPlugInHostRef, CFStringRef) {
    if (gStoredValue != nullptr) {
        CFRelease(gStoredValue);
        gStoredValue = nullptr;
    }
    return noErr;
}

OSStatus RequestDeviceConfigurationChange(AudioServerPlugInHostRef, AudioObjectID device,
                                          UInt64 action, void* changeInfo) {
    ++gConfigurationRequests;
    if (gDriver == nullptr || gPendingDevice != kAudioObjectUnknown) {
        return kAudioHardwareNotReadyError;
    }
    gPendingDevice = device;
    gPendingAction = action;
    gPendingChangeInfo = changeInfo;
    return noErr;
}

AudioServerPlugInHostInterface gHost = {
    PropertiesChanged,
    CopyFromStorage,
    WriteToStorage,
    DeleteFromStorage,
    RequestDeviceConfigurationChange,
};

bool CheckStatus(OSStatus status, const char* operation) {
    if (status == noErr) {
        return true;
    }
    std::fprintf(stderr, "%s failed with OSStatus %d ('%c%c%c%c')\n", operation, status,
                 static_cast<char>((static_cast<UInt32>(status) >> 24) & 0xff),
                 static_cast<char>((static_cast<UInt32>(status) >> 16) & 0xff),
                 static_cast<char>((static_cast<UInt32>(status) >> 8) & 0xff),
                 static_cast<char>(static_cast<UInt32>(status) & 0xff));
    return false;
}

bool PerformPendingConfigurationChange() {
    if (gDriver == nullptr || gPendingDevice == kAudioObjectUnknown) {
        std::fprintf(stderr, "no pending configuration change\n");
        return false;
    }
    const AudioObjectID device = gPendingDevice;
    const UInt64 action = gPendingAction;
    void* const changeInfo = gPendingChangeInfo;
    gPendingDevice = kAudioObjectUnknown;
    gPendingAction = 0;
    gPendingChangeInfo = nullptr;
    return CheckStatus((*gDriver)->PerformDeviceConfigurationChange(
                           gDriver, device, action, changeInfo),
                       "PerformDeviceConfigurationChange");
}

template <typename T>
bool GetValue(AudioServerPlugInDriverRef driver, AudioObjectID object,
              AudioObjectPropertySelector selector, AudioObjectPropertyScope scope, T& value) {
    const AudioObjectPropertyAddress address = {selector, scope, kAudioObjectPropertyElementMain};
    UInt32 size = sizeof(value);
    UInt32 used = 0;
    return CheckStatus((*driver)->GetPropertyData(driver, object, 0, &address, 0, nullptr, size,
                                                   &used, &value),
                       "GetPropertyData") &&
           used == sizeof(value);
}

OSStatus SetControlStatus(AudioServerPlugInDriverRef driver, const char* command,
                          pid_t clientProcessID = 0) {
    CFStringRef value = CFStringCreateWithCString(kCFAllocatorDefault, command,
                                                   kCFStringEncodingUTF8);
    if (value == nullptr) {
        return kAudioHardwareUnspecifiedError;
    }
    const AudioObjectPropertyAddress address = {kVCableControlProperty,
                                                kAudioObjectPropertyScopeGlobal,
                                                kAudioObjectPropertyElementMain};
    const OSStatus status = (*driver)->SetPropertyData(
        driver, kAudioObjectPlugInObject, clientProcessID, &address, 0, nullptr,
        sizeof(value), &value);
    CFRelease(value);
    return status;
}

bool SetControl(AudioServerPlugInDriverRef driver, const char* command,
                pid_t clientProcessID = 0) {
    const OSStatus status = SetControlStatus(driver, command, clientProcessID);
    if (!CheckStatus(status, "SetPropertyData(vctl)")) {
        return false;
    }
    return std::string_view(command).starts_with("delete\t")
        ? PerformPendingConfigurationChange()
        : true;
}

bool GetDeviceList(AudioServerPlugInDriverRef driver, AudioObjectID& device) {
    const AudioObjectPropertyAddress address = {kAudioPlugInPropertyDeviceList,
                                                kAudioObjectPropertyScopeGlobal,
                                                kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    if (!CheckStatus((*driver)->GetPropertyDataSize(driver, kAudioObjectPlugInObject, 0, &address,
                                                    0, nullptr, &size),
                     "GetPropertyDataSize(device list)")) {
        return false;
    }
    if (size != sizeof(device)) {
        std::fprintf(stderr, "expected one device, received %u bytes\n", size);
        return false;
    }
    UInt32 used = 0;
    return CheckStatus((*driver)->GetPropertyData(driver, kAudioObjectPlugInObject, 0, &address, 0,
                                                   nullptr, size, &used, &device),
                       "GetPropertyData(device list)") &&
           used == sizeof(device);
}

bool GetDeviceCount(AudioServerPlugInDriverRef driver, UInt32 expectedCount) {
    const AudioObjectPropertyAddress address = {kAudioPlugInPropertyDeviceList,
                                                kAudioObjectPropertyScopeGlobal,
                                                kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    return CheckStatus((*driver)->GetPropertyDataSize(driver, kAudioObjectPlugInObject, 0, &address,
                                                       0, nullptr, &size),
                       "GetPropertyDataSize(device count)") &&
           size == expectedCount * sizeof(AudioObjectID);
}

bool GetStreams(AudioServerPlugInDriverRef driver, AudioObjectID device, AudioObjectID& input,
                AudioObjectID& output) {
    const AudioObjectPropertyAddress address = {kAudioDevicePropertyStreams,
                                                kAudioObjectPropertyScopeGlobal,
                                                kAudioObjectPropertyElementMain};
    AudioObjectID streams[2] = {};
    UInt32 used = 0;
    if (!CheckStatus((*driver)->GetPropertyData(driver, device, 0, &address, 0, nullptr,
                                                sizeof(streams), &used, streams),
                     "GetPropertyData(streams)") ||
        used != sizeof(streams)) {
        return false;
    }
    input = streams[0];
    output = streams[1];
    return true;
}

bool VerifyProperties(AudioServerPlugInDriverRef driver, AudioObjectID device,
                      AudioObjectID inputStream, AudioObjectID outputStream) {
    Float64 sampleRate = 0.0;
    UInt32 transport = 0;
    UInt32 inputDirection = 0;
    UInt32 outputDirection = 1;
    AudioStreamBasicDescription inputFormat{};
    AudioStreamBasicDescription outputFormat{};
    return GetValue(driver, device, kAudioDevicePropertyNominalSampleRate,
                    kAudioObjectPropertyScopeGlobal, sampleRate) &&
           sampleRate == 48000.0 &&
           GetValue(driver, device, kAudioDevicePropertyTransportType,
                    kAudioObjectPropertyScopeGlobal, transport) &&
           transport == kAudioDeviceTransportTypeVirtual &&
           GetValue(driver, inputStream, kAudioStreamPropertyDirection,
                    kAudioObjectPropertyScopeGlobal, inputDirection) &&
           inputDirection == 1 &&
           GetValue(driver, outputStream, kAudioStreamPropertyDirection,
                    kAudioObjectPropertyScopeGlobal, outputDirection) &&
           outputDirection == 0 &&
           GetValue(driver, inputStream, kAudioStreamPropertyVirtualFormat,
                    kAudioObjectPropertyScopeGlobal, inputFormat) &&
           inputFormat.mChannelsPerFrame == 2 && inputFormat.mFormatID == kAudioFormatLinearPCM &&
           GetValue(driver, outputStream, kAudioStreamPropertyVirtualFormat,
                    kAudioObjectPropertyScopeGlobal, outputFormat) &&
           outputFormat.mChannelsPerFrame == 2 && outputFormat.mFormatID == kAudioFormatLinearPCM;
}

bool VerifyLoopback(AudioServerPlugInDriverRef driver, AudioObjectID device,
                    AudioObjectID inputStream, AudioObjectID outputStream) {
    AudioServerPlugInClientInfo client{};
    client.mClientID = 1;
    if (!CheckStatus((*driver)->AddDeviceClient(driver, device, &client), "AddDeviceClient") ||
        !CheckStatus((*driver)->StartIO(driver, device, 1), "StartIO")) {
        return false;
    }

    constexpr UInt32 frames = 4;
    Float32 written[frames * 2] = {0.25F, -0.25F, 0.5F, -0.5F,
                                   0.75F, -0.75F, 1.0F, -1.0F};
    Float32 read[frames * 2] = {};
    AudioServerPlugInIOCycleInfo cycle{};
    cycle.mOutputTime.mSampleTime = 512.0;
    cycle.mInputTime.mSampleTime = 1024.0;
    if (!CheckStatus((*driver)->DoIOOperation(
                         driver, device, outputStream, 1,
                         kAudioServerPlugInIOOperationWriteMix, frames, &cycle, written, nullptr),
                     "DoIOOperation(write)") ||
        !CheckStatus((*driver)->DoIOOperation(
                         driver, device, inputStream, 1,
                         kAudioServerPlugInIOOperationReadInput, frames, &cycle, read, nullptr),
                     "DoIOOperation(read)")) {
        return false;
    }
    if (std::memcmp(written, read, sizeof(written)) != 0) {
        std::fprintf(stderr, "loopback samples differ\n");
        return false;
    }

    Float64 sampleTime = 0.0;
    UInt64 hostTime = 0;
    UInt64 seed = 0;
    const bool timestamp = CheckStatus(
        (*driver)->GetZeroTimeStamp(driver, device, 1, &sampleTime, &hostTime, &seed),
        "GetZeroTimeStamp");
    const bool stopped = CheckStatus((*driver)->StopIO(driver, device, 1), "StopIO") &&
                         CheckStatus((*driver)->RemoveDeviceClient(driver, device, &client),
                                     "RemoveDeviceClient");
    return timestamp && hostTime != 0 && seed != 0 && stopped;
}

bool VerifyDeletionGuards(AudioServerPlugInDriverRef driver) {
    if (!SetControl(driver, "create\tguarded\tVCable Guarded\t2\t2\t48000")) {
        return false;
    }
    AudioObjectID device = 0;
    if (!GetDeviceList(driver, device)) {
        return false;
    }
    AudioServerPlugInClientInfo client{};
    client.mClientID = 27;
    client.mProcessID = 1001;
    if (!CheckStatus((*driver)->AddDeviceClient(driver, device, &client),
                     "AddDeviceClient(guarded)")) {
        return false;
    }
    const bool started =
        CheckStatus((*driver)->StartIO(driver, device, client.mClientID),
                    "StartIO(guarded)");
    const bool rejectsRunningClient =
        SetControlStatus(driver, "delete\tguarded", 2002) ==
        kAudioHardwareIllegalOperationError;
    const bool stopped =
        CheckStatus((*driver)->StopIO(driver, device, client.mClientID),
                    "StopIO(guarded)");
    const bool acceptsDeleteRequest =
        SetControlStatus(driver, "delete\tguarded", 2002) == noErr;
    const bool remainsUntilHostApproval = GetDeviceCount(driver, 1);
    const bool acceptsAttachedClient = PerformPendingConfigurationChange();
    const bool detached = CheckStatus(
        (*driver)->RemoveDeviceClient(driver, device, &client),
        "RemoveDeviceClient(guarded)");
    return started && rejectsRunningClient && stopped && acceptsDeleteRequest &&
           remainsUntilHostApproval && acceptsAttachedClient && detached &&
           GetDeviceCount(driver, 0);
}

}  // namespace

int main(int argc, char** argv) {
    if (argc != 2) {
        std::fprintf(stderr, "usage: VCableDriverTests PATH_TO_DRIVER_BINARY\n");
        return 2;
    }
    void* bundle = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
    if (bundle == nullptr) {
        std::fprintf(stderr, "dlopen failed: %s\n", dlerror());
        return 1;
    }
    using Factory = void* (*)(CFAllocatorRef, CFUUIDRef);
    auto factory = reinterpret_cast<Factory>(dlsym(bundle, "VCableFactory"));
    if (factory == nullptr) {
        std::fprintf(stderr, "VCableFactory was not exported\n");
        dlclose(bundle);
        return 1;
    }
    auto driver = static_cast<AudioServerPlugInDriverRef>(
        factory(kCFAllocatorDefault, kAudioServerPlugInTypeUUID));
    gDriver = driver;
    gStoredValue = CFStringCreateWithCString(
        kCFAllocatorDefault, "create\tpersisted\tVCable Persisted\t2\t2\t48000\n",
        kCFStringEncodingUTF8);
    if (driver == nullptr ||
        gStoredValue == nullptr ||
        !CheckStatus((*driver)->Initialize(driver, &gHost), "Initialize") ||
        !GetDeviceCount(driver, 1) || !SetControl(driver, "delete\tpersisted") ||
        !VerifyDeletionGuards(driver) ||
        !SetControl(driver, "create\ttest\tVCable Test\t2\t2\t48000")) {
        dlclose(bundle);
        return 1;
    }

    AudioObjectID device = 0;
    AudioObjectID inputStream = 0;
    AudioObjectID outputStream = 0;
    const bool passed =
        GetDeviceList(driver, device) && GetStreams(driver, device, inputStream, outputStream) &&
        VerifyProperties(driver, device, inputStream, outputStream) &&
        VerifyLoopback(driver, device, inputStream, outputStream) &&
        SetControl(driver, "create\tmono\tVCable Mono\t1\t1\t44100") &&
        SetControl(driver, "create\tquad\tVCable Quad\t4\t4\t96000") &&
        GetDeviceCount(driver, 3) && SetControl(driver, "delete\tmono") &&
        SetControl(driver, "delete\tquad") && SetControl(driver, "delete\ttest") &&
        GetDeviceCount(driver, 0) && gNotifications >= 14;

    if (gStoredValue != nullptr) {
        CFRelease(gStoredValue);
        gStoredValue = nullptr;
    }
    (*driver)->Release(driver);
    gDriver = nullptr;
    dlclose(bundle);
    if (!passed) {
        return 1;
    }
    if (gConfigurationRequests != 5 || gPendingDevice != kAudioObjectUnknown) {
        std::fprintf(stderr, "expected 5 configuration requests, got %u\n",
                     gConfigurationRequests);
        return 1;
    }
    std::puts("VCableDriver integration test passed");
    return 0;
}
