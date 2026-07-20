#include <CoreAudio/AudioHardware.h>
#include <CoreAudio/AudioServerPlugIn.h>
#include <CoreFoundation/CoreFoundation.h>
#include <mach/mach_time.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <limits>
#include <memory>
#include <mutex>
#include <new>
#include <sstream>
#include <string>
#include <string_view>
#include <vector>

namespace {

constexpr AudioObjectID kFirstDeviceObjectID = 100;
constexpr UInt32 kObjectsPerDevice = 3;
constexpr size_t kMaximumDevices = 4096;
constexpr UInt32 kMaximumChannels = 64;
constexpr UInt32 kRingFrames = 65536;
constexpr UInt32 kZeroTimeStampPeriod = 16384;
constexpr UInt32 kLoopbackLatencyFrames = 512;
constexpr UInt32 kBufferFrameSize = 512;
constexpr UInt64 kDeleteDeviceConfigurationAction = 1;
constexpr AudioObjectPropertySelector kVCableControlProperty = 'vctl';
constexpr AudioObjectPropertySelector kVCableStatusProperty = 'vsta';
constexpr const char* kStorageKey = "devices-v1";
constexpr const char* kManufacturer = "VCable Project";
constexpr const char* kModelUID = "dev.vcable.model";
constexpr const char* kBundleID = "dev.vcable.driver";
constexpr const char* kUIDPrefix = "dev.vcable.device.";

struct Device {
    Device(size_t slotNumber, std::string deviceID, std::string deviceName,
           UInt32 inputChannelCount, UInt32 outputChannelCount, Float64 rate)
        : slot(slotNumber), id(std::move(deviceID)), name(std::move(deviceName)),
          uid(std::string(kUIDPrefix) + id), inputChannels(inputChannelCount),
          outputChannels(outputChannelCount), sampleRate(rate),
          sampleStride(std::max(inputChannels, outputChannels)),
          samples(std::make_unique<std::atomic<UInt32>[]>(
              static_cast<size_t>(kRingFrames) * sampleStride)),
          tags(std::make_unique<std::atomic<UInt64>[]>(kRingFrames)) {
        nameString = CFStringCreateWithCString(kCFAllocatorDefault, name.c_str(),
                                                kCFStringEncodingUTF8);
        uidString = CFStringCreateWithCString(kCFAllocatorDefault, uid.c_str(),
                                               kCFStringEncodingUTF8);
        for (size_t index = 0;
             index < static_cast<size_t>(kRingFrames) * sampleStride; ++index) {
            samples[index].store(0, std::memory_order_relaxed);
        }
        ClearTags();
    }

    ~Device() {
        if (nameString != nullptr) {
            CFRelease(nameString);
        }
        if (uidString != nullptr) {
            CFRelease(uidString);
        }
    }

    Device(const Device&) = delete;
    Device& operator=(const Device&) = delete;

    AudioObjectID DeviceObjectID() const {
        return kFirstDeviceObjectID + static_cast<AudioObjectID>(slot * kObjectsPerDevice);
    }
    AudioObjectID InputStreamObjectID() const { return DeviceObjectID() + 1; }
    AudioObjectID OutputStreamObjectID() const { return DeviceObjectID() + 2; }

    void ClearTags() {
        for (UInt32 index = 0; index < kRingFrames; ++index) {
            tags[index].store(0, std::memory_order_relaxed);
        }
    }

    void Write(UInt64 sampleTime, UInt32 frames, const Float32* input) {
        if (input == nullptr || !outputActive.load(std::memory_order_relaxed)) {
            return;
        }
        for (UInt32 frame = 0; frame < frames; ++frame) {
            const UInt64 absoluteFrame = sampleTime + frame;
            const size_t ringFrame = static_cast<size_t>(absoluteFrame % kRingFrames);
            const size_t base = ringFrame * sampleStride;
            for (UInt32 channel = 0; channel < sampleStride; ++channel) {
                const Float32 value = channel < outputChannels
                    ? input[static_cast<size_t>(frame) * outputChannels + channel]
                    : 0.0F;
                UInt32 bits = 0;
                static_assert(sizeof(bits) == sizeof(value));
                std::memcpy(&bits, &value, sizeof(bits));
                samples[base + channel].store(bits, std::memory_order_relaxed);
            }
            tags[ringFrame].store(absoluteFrame + 1, std::memory_order_release);
        }
    }

    void Read(UInt64 sampleTime, UInt32 frames, Float32* output) const {
        if (output == nullptr) {
            return;
        }
        const size_t sampleCount = static_cast<size_t>(frames) * inputChannels;
        std::fill_n(output, sampleCount, 0.0F);
        if (!inputActive.load(std::memory_order_relaxed)) {
            return;
        }
        for (UInt32 frame = 0; frame < frames; ++frame) {
            const UInt64 inputFrame = sampleTime + frame;
            if (inputFrame < kLoopbackLatencyFrames) {
                continue;
            }
            const UInt64 sourceFrame = inputFrame - kLoopbackLatencyFrames;
            const size_t ringFrame = static_cast<size_t>(sourceFrame % kRingFrames);
            if (tags[ringFrame].load(std::memory_order_acquire) != sourceFrame + 1) {
                underruns.fetch_add(1, std::memory_order_relaxed);
                continue;
            }
            const size_t base = ringFrame * sampleStride;
            const UInt32 channelsToCopy = std::min(inputChannels, outputChannels);
            for (UInt32 channel = 0; channel < channelsToCopy; ++channel) {
                const UInt32 bits = samples[base + channel].load(std::memory_order_relaxed);
                Float32 value = 0.0F;
                std::memcpy(&value, &bits, sizeof(value));
                output[static_cast<size_t>(frame) * inputChannels + channel] = value;
            }
        }
    }

    size_t slot;
    std::string id;
    std::string name;
    std::string uid;
    UInt32 inputChannels;
    UInt32 outputChannels;
    Float64 sampleRate;
    UInt32 sampleStride;
    CFStringRef nameString = nullptr;
    CFStringRef uidString = nullptr;
    std::unique_ptr<std::atomic<UInt32>[]> samples;
    std::unique_ptr<std::atomic<UInt64>[]> tags;
    std::atomic<bool> active{true};
    std::atomic<bool> deletePending{false};
    std::atomic<UInt32> attachedClients{0};
    std::atomic<UInt32> runningClients{0};
    std::atomic<bool> inputActive{true};
    std::atomic<bool> outputActive{true};
    std::atomic<UInt64> anchorHostTime{0};
    std::atomic<UInt64> clockSeed{1};
    mutable std::atomic<UInt64> underruns{0};
};

enum class ObjectKind { Invalid, PlugIn, Device, InputStream, OutputStream };

std::mutex gStateMutex;
std::atomic<UInt32> gReferenceCount{1};
AudioServerPlugInHostRef gHost = nullptr;
std::array<std::atomic<Device*>, kMaximumDevices> gSlots{};
std::vector<std::unique_ptr<Device>> gAllDevices;
std::vector<Device*> gActiveDevices;
Float64 gHostTicksPerSecond = 0.0;

HRESULT QueryInterface(void*, REFIID, LPVOID*);
ULONG AddRef(void*);
ULONG Release(void*);
OSStatus Initialize(AudioServerPlugInDriverRef, AudioServerPlugInHostRef);
OSStatus CreateDevice(AudioServerPlugInDriverRef, CFDictionaryRef,
                      const AudioServerPlugInClientInfo*, AudioObjectID*);
OSStatus DestroyDevice(AudioServerPlugInDriverRef, AudioObjectID);
OSStatus AddDeviceClient(AudioServerPlugInDriverRef, AudioObjectID,
                         const AudioServerPlugInClientInfo*);
OSStatus RemoveDeviceClient(AudioServerPlugInDriverRef, AudioObjectID,
                            const AudioServerPlugInClientInfo*);
OSStatus PerformDeviceConfigurationChange(AudioServerPlugInDriverRef, AudioObjectID,
                                           UInt64, void*);
OSStatus AbortDeviceConfigurationChange(AudioServerPlugInDriverRef, AudioObjectID,
                                         UInt64, void*);
Boolean HasProperty(AudioServerPlugInDriverRef, AudioObjectID, pid_t,
                    const AudioObjectPropertyAddress*);
OSStatus IsPropertySettable(AudioServerPlugInDriverRef, AudioObjectID, pid_t,
                            const AudioObjectPropertyAddress*, Boolean*);
OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef, AudioObjectID, pid_t,
                             const AudioObjectPropertyAddress*, UInt32, const void*, UInt32*);
OSStatus GetPropertyData(AudioServerPlugInDriverRef, AudioObjectID, pid_t,
                         const AudioObjectPropertyAddress*, UInt32, const void*, UInt32, UInt32*,
                         void*);
OSStatus SetPropertyData(AudioServerPlugInDriverRef, AudioObjectID, pid_t,
                         const AudioObjectPropertyAddress*, UInt32, const void*, UInt32,
                         const void*);
OSStatus StartIO(AudioServerPlugInDriverRef, AudioObjectID, UInt32);
OSStatus StopIO(AudioServerPlugInDriverRef, AudioObjectID, UInt32);
OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef, AudioObjectID, UInt32, Float64*, UInt64*,
                          UInt64*);
OSStatus WillDoIOOperation(AudioServerPlugInDriverRef, AudioObjectID, UInt32, UInt32, Boolean*,
                           Boolean*);
OSStatus BeginIOOperation(AudioServerPlugInDriverRef, AudioObjectID, UInt32, UInt32, UInt32,
                          const AudioServerPlugInIOCycleInfo*);
OSStatus DoIOOperation(AudioServerPlugInDriverRef, AudioObjectID, AudioObjectID, UInt32, UInt32,
                       UInt32, const AudioServerPlugInIOCycleInfo*, void*, void*);
OSStatus EndIOOperation(AudioServerPlugInDriverRef, AudioObjectID, UInt32, UInt32, UInt32,
                        const AudioServerPlugInIOCycleInfo*);

AudioServerPlugInDriverInterface gInterface = {
    nullptr,
    QueryInterface,
    AddRef,
    Release,
    Initialize,
    CreateDevice,
    DestroyDevice,
    AddDeviceClient,
    RemoveDeviceClient,
    PerformDeviceConfigurationChange,
    AbortDeviceConfigurationChange,
    HasProperty,
    IsPropertySettable,
    GetPropertyDataSize,
    GetPropertyData,
    SetPropertyData,
    StartIO,
    StopIO,
    GetZeroTimeStamp,
    WillDoIOOperation,
    BeginIOOperation,
    DoIOOperation,
    EndIOOperation,
};
AudioServerPlugInDriverInterface* gInterfacePointer = &gInterface;
AudioServerPlugInDriverRef gDriverRef = &gInterfacePointer;

bool IsDriver(AudioServerPlugInDriverRef driver) { return driver == gDriverRef; }

Device* DeviceForObject(AudioObjectID objectID) {
    if (objectID < kFirstDeviceObjectID) {
        return nullptr;
    }
    const UInt32 offset = objectID - kFirstDeviceObjectID;
    const size_t slot = offset / kObjectsPerDevice;
    if (slot >= kMaximumDevices) {
        return nullptr;
    }
    Device* device = gSlots[slot].load(std::memory_order_acquire);
    if (device == nullptr || objectID > device->OutputStreamObjectID()) {
        return nullptr;
    }
    return device;
}

ObjectKind KindForObject(AudioObjectID objectID) {
    if (objectID == kAudioObjectPlugInObject) {
        return ObjectKind::PlugIn;
    }
    Device* device = DeviceForObject(objectID);
    if (device == nullptr) {
        return ObjectKind::Invalid;
    }
    if (objectID == device->DeviceObjectID()) {
        return ObjectKind::Device;
    }
    if (objectID == device->InputStreamObjectID()) {
        return ObjectKind::InputStream;
    }
    return ObjectKind::OutputStream;
}

OSStatus PutBytes(UInt32 inputSize, UInt32 requiredSize, UInt32* outputSize, void* output,
                  const void* value) {
    if (outputSize == nullptr || output == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    if (inputSize < requiredSize) {
        return kAudioHardwareBadPropertySizeError;
    }
    if (requiredSize > 0) {
        std::memcpy(output, value, requiredSize);
    }
    *outputSize = requiredSize;
    return noErr;
}

template <typename T>
OSStatus PutValue(UInt32 inputSize, UInt32* outputSize, void* output, const T& value) {
    return PutBytes(inputSize, sizeof(T), outputSize, output, &value);
}

OSStatus PutString(UInt32 inputSize, UInt32* outputSize, void* output, CFStringRef value) {
    if (value == nullptr) {
        return kAudioHardwareUnspecifiedError;
    }
    CFRetain(value);
    const OSStatus status = PutValue(inputSize, outputSize, output, value);
    if (status != noErr) {
        CFRelease(value);
    }
    return status;
}

CFStringRef MakeString(const std::string& value) {
    return CFStringCreateWithBytes(kCFAllocatorDefault,
                                   reinterpret_cast<const UInt8*>(value.data()), value.size(),
                                   kCFStringEncodingUTF8, false);
}

bool ToString(CFStringRef value, std::string& result) {
    if (value == nullptr) {
        return false;
    }
    const CFIndex length = CFStringGetLength(value);
    const CFIndex maximum = CFStringGetMaximumSizeForEncoding(length, kCFStringEncodingUTF8) + 1;
    if (maximum <= 0) {
        return false;
    }
    std::vector<char> buffer(static_cast<size_t>(maximum));
    if (!CFStringGetCString(value, buffer.data(), maximum, kCFStringEncodingUTF8)) {
        return false;
    }
    result.assign(buffer.data());
    return true;
}

std::vector<std::string> Split(std::string_view value, char delimiter) {
    std::vector<std::string> result;
    size_t start = 0;
    while (start <= value.size()) {
        const size_t end = value.find(delimiter, start);
        result.emplace_back(value.substr(start, end == std::string_view::npos ? value.size() - start
                                                                              : end - start));
        if (end == std::string_view::npos) {
            break;
        }
        start = end + 1;
    }
    return result;
}

bool ValidID(const std::string& value) {
    if (value.empty() || value.size() > 128) {
        return false;
    }
    return std::all_of(value.begin(), value.end(), [](unsigned char character) {
        return std::isalnum(character) != 0 || character == '.' || character == '_' ||
               character == '-';
    });
}

bool ValidName(const std::string& value) {
    return !value.empty() && value.size() <= 255 && value.find_first_of("\t\r\n\0") == std::string::npos;
}

bool ParseUInt32(const std::string& value, UInt32& result) {
    if (value.empty()) {
        return false;
    }
    UInt64 parsed = 0;
    for (const char character : value) {
        if (character < '0' || character > '9') {
            return false;
        }
        parsed = parsed * 10 + static_cast<UInt64>(character - '0');
        if (parsed > std::numeric_limits<UInt32>::max()) {
            return false;
        }
    }
    result = static_cast<UInt32>(parsed);
    return true;
}

Device* FindByIDLocked(const std::string& id) {
    const auto found = std::find_if(gActiveDevices.begin(), gActiveDevices.end(),
                                    [&id](const Device* device) { return device->id == id; });
    return found == gActiveDevices.end() ? nullptr : *found;
}

Device* FindByUIDLocked(CFStringRef uid) {
    const auto found = std::find_if(gActiveDevices.begin(), gActiveDevices.end(),
                                    [uid](const Device* device) {
                                        return CFEqual(device->uidString, uid);
                                    });
    return found == gActiveDevices.end() ? nullptr : *found;
}

std::string InventoryLocked() {
    std::ostringstream output;
    for (const Device* device : gActiveDevices) {
        output << "create\t" << device->id << '\t' << device->name << '\t'
               << device->inputChannels << '\t' << device->outputChannels << '\t'
               << static_cast<UInt32>(device->sampleRate) << '\n';
    }
    return output.str();
}

OSStatus PersistLocked() {
    if (gHost == nullptr) {
        return kAudioHardwareNotReadyError;
    }
    CFStringRef key = CFStringCreateWithCString(kCFAllocatorDefault, kStorageKey,
                                                kCFStringEncodingUTF8);
    CFStringRef value = MakeString(InventoryLocked());
    if (key == nullptr || value == nullptr) {
        if (key != nullptr) {
            CFRelease(key);
        }
        if (value != nullptr) {
            CFRelease(value);
        }
        return kAudioHardwareUnspecifiedError;
    }
    const OSStatus status = gHost->WriteToStorage(gHost, key, value);
    CFRelease(value);
    CFRelease(key);
    return status;
}

OSStatus AddDeviceLocked(const std::vector<std::string>& fields, bool persist, Device** created) {
    if (fields.size() != 6 || fields[0] != "create" || !ValidID(fields[1]) ||
        !ValidName(fields[2])) {
        return kAudioHardwareIllegalOperationError;
    }
    UInt32 inputChannels = 0;
    UInt32 outputChannels = 0;
    UInt32 sampleRate = 0;
    if (!ParseUInt32(fields[3], inputChannels) || !ParseUInt32(fields[4], outputChannels) ||
        !ParseUInt32(fields[5], sampleRate) || inputChannels == 0 || outputChannels == 0 ||
        inputChannels > kMaximumChannels || outputChannels > kMaximumChannels ||
        sampleRate < 8000 || sampleRate > 768000) {
        return kAudioHardwareIllegalOperationError;
    }
    if (Device* existing = FindByIDLocked(fields[1]); existing != nullptr) {
        const bool matches = existing->name == fields[2] &&
                             existing->inputChannels == inputChannels &&
                             existing->outputChannels == outputChannels &&
                             existing->sampleRate == sampleRate;
        if (created != nullptr) {
            *created = matches ? existing : nullptr;
        }
        return matches ? static_cast<OSStatus>(noErr)
                       : static_cast<OSStatus>(kAudioHardwareIllegalOperationError);
    }
    const auto freeSlot = std::find_if(gSlots.begin(), gSlots.end(), [](const auto& slot) {
        return slot.load(std::memory_order_relaxed) == nullptr;
    });
    if (freeSlot == gSlots.end()) {
        return kAudioHardwareIllegalOperationError;
    }
    const size_t slot = static_cast<size_t>(freeSlot - gSlots.begin());
    std::unique_ptr<Device> device;
    try {
        device = std::make_unique<Device>(slot, fields[1], fields[2], inputChannels,
                                          outputChannels, sampleRate);
    } catch (const std::bad_alloc&) {
        return kAudioHardwareUnspecifiedError;
    }
    if (device->nameString == nullptr || device->uidString == nullptr) {
        return kAudioHardwareUnspecifiedError;
    }
    Device* pointer = device.get();
    gAllDevices.push_back(std::move(device));
    gActiveDevices.push_back(pointer);
    gSlots[slot].store(pointer, std::memory_order_release);
    if (persist) {
        const OSStatus status = PersistLocked();
        if (status != noErr) {
            gSlots[slot].store(nullptr, std::memory_order_release);
            gActiveDevices.pop_back();
            return status;
        }
    }
    if (created != nullptr) {
        *created = pointer;
    }
    return noErr;
}

OSStatus RemoveDeviceLocked(Device* device, bool persist) {
    if (device == nullptr) {
        return kAudioHardwareBadObjectError;
    }
    if (device->runningClients.load(std::memory_order_acquire) != 0) {
        return kAudioHardwareIllegalOperationError;
    }
    const auto found = std::find(gActiveDevices.begin(), gActiveDevices.end(), device);
    if (found == gActiveDevices.end()) {
        return kAudioHardwareBadObjectError;
    }
    const size_t position = static_cast<size_t>(found - gActiveDevices.begin());
    gActiveDevices.erase(found);
    if (persist) {
        const OSStatus status = PersistLocked();
        if (status != noErr) {
            gActiveDevices.insert(gActiveDevices.begin() + static_cast<std::ptrdiff_t>(position),
                                  device);
            return status;
        }
    }
    device->active.store(false, std::memory_order_release);
    return noErr;
}

void ReleaseRetiredSlotLocked(Device* device) {
    if (device == nullptr || device->active.load(std::memory_order_acquire) ||
        device->attachedClients.load(std::memory_order_acquire) != 0 ||
        device->runningClients.load(std::memory_order_acquire) != 0) {
        return;
    }
    if (gSlots[device->slot].load(std::memory_order_acquire) == device) {
        gSlots[device->slot].store(nullptr, std::memory_order_release);
    }
}

void NotifyDeviceListChanged() {
    if (gHost == nullptr) {
        return;
    }
    const AudioObjectPropertyAddress addresses[] = {
        {kAudioObjectPropertyOwnedObjects, kAudioObjectPropertyScopeGlobal,
         kAudioObjectPropertyElementMain},
        {kAudioPlugInPropertyDeviceList, kAudioObjectPropertyScopeGlobal,
         kAudioObjectPropertyElementMain},
    };
    gHost->PropertiesChanged(gHost, kAudioObjectPlugInObject, 2, addresses);
}

AudioStreamBasicDescription StreamFormat(const Device& device, bool input) {
    const UInt32 channels = input ? device.inputChannels : device.outputChannels;
    AudioStreamBasicDescription format{};
    format.mSampleRate = device.sampleRate;
    format.mFormatID = kAudioFormatLinearPCM;
    format.mFormatFlags = static_cast<AudioFormatFlags>(kAudioFormatFlagIsFloat) |
                          static_cast<AudioFormatFlags>(kAudioFormatFlagsNativeEndian) |
                          static_cast<AudioFormatFlags>(kAudioFormatFlagIsPacked);
    format.mBytesPerPacket = channels * sizeof(Float32);
    format.mFramesPerPacket = 1;
    format.mBytesPerFrame = channels * sizeof(Float32);
    format.mChannelsPerFrame = channels;
    format.mBitsPerChannel = 8 * sizeof(Float32);
    return format;
}

UInt32 StreamCountForScope(const AudioObjectPropertyAddress& address) {
    if (address.mScope == kAudioObjectPropertyScopeGlobal) {
        return 2;
    }
    if (address.mScope == kAudioObjectPropertyScopeInput ||
        address.mScope == kAudioObjectPropertyScopeOutput) {
        return 1;
    }
    return 0;
}

UInt32 ChannelsForScope(const Device& device, AudioObjectPropertyScope scope) {
    if (scope == kAudioObjectPropertyScopeInput) {
        return device.inputChannels;
    }
    if (scope == kAudioObjectPropertyScopeOutput) {
        return device.outputChannels;
    }
    return device.inputChannels + device.outputChannels;
}

bool PlugInHas(AudioObjectPropertySelector selector) {
    switch (selector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        case kAudioObjectPropertyManufacturer:
        case kAudioObjectPropertyOwnedObjects:
        case kAudioObjectPropertyCustomPropertyInfoList:
        case kAudioPlugInPropertyBundleID:
        case kAudioPlugInPropertyDeviceList:
        case kAudioPlugInPropertyTranslateUIDToDevice:
        case kAudioPlugInPropertyBoxList:
        case kAudioPlugInPropertyResourceBundle:
        case kVCableControlProperty:
        case kVCableStatusProperty:
            return true;
        default:
            return false;
    }
}

bool DeviceHas(AudioObjectPropertySelector selector) {
    switch (selector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        case kAudioObjectPropertyName:
        case kAudioObjectPropertyManufacturer:
        case kAudioObjectPropertyOwnedObjects:
        case kAudioDevicePropertyDeviceUID:
        case kAudioDevicePropertyModelUID:
        case kAudioDevicePropertyTransportType:
        case kAudioDevicePropertyRelatedDevices:
        case kAudioDevicePropertyClockDomain:
        case kAudioDevicePropertyDeviceIsAlive:
        case kAudioDevicePropertyDeviceIsRunning:
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
        case kAudioDevicePropertyLatency:
        case kAudioDevicePropertyStreams:
        case kAudioObjectPropertyControlList:
        case kAudioDevicePropertySafetyOffset:
        case kAudioDevicePropertyNominalSampleRate:
        case kAudioDevicePropertyAvailableNominalSampleRates:
        case kAudioDevicePropertyIsHidden:
        case kAudioDevicePropertyPreferredChannelsForStereo:
        case kAudioDevicePropertyStreamConfiguration:
        case kAudioDevicePropertyZeroTimeStampPeriod:
        case kAudioDevicePropertyClockIsStable:
        case kAudioDevicePropertyBufferFrameSize:
        case kAudioDevicePropertyBufferFrameSizeRange:
            return true;
        default:
            return false;
    }
}

bool StreamHas(AudioObjectPropertySelector selector) {
    switch (selector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
        case kAudioObjectPropertyOwner:
        case kAudioObjectPropertyOwnedObjects:
        case kAudioStreamPropertyIsActive:
        case kAudioStreamPropertyDirection:
        case kAudioStreamPropertyTerminalType:
        case kAudioStreamPropertyStartingChannel:
        case kAudioStreamPropertyLatency:
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat:
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats:
            return true;
        default:
            return false;
    }
}

OSStatus PlugInDataSize(const AudioObjectPropertyAddress& address, UInt32* size) {
    if (size == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    std::lock_guard lock(gStateMutex);
    switch (address.mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
            *size = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyOwner:
            *size = sizeof(AudioObjectID);
            break;
        case kAudioObjectPropertyManufacturer:
        case kAudioPlugInPropertyBundleID:
        case kAudioPlugInPropertyResourceBundle:
        case kVCableControlProperty:
        case kVCableStatusProperty:
            *size = sizeof(CFStringRef);
            break;
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList:
            *size = static_cast<UInt32>(gActiveDevices.size() * sizeof(AudioObjectID));
            break;
        case kAudioObjectPropertyCustomPropertyInfoList:
            *size = 2 * sizeof(AudioServerPlugInCustomPropertyInfo);
            break;
        case kAudioPlugInPropertyTranslateUIDToDevice:
            *size = sizeof(AudioObjectID);
            break;
        case kAudioPlugInPropertyBoxList:
            *size = 0;
            break;
        default:
            return kAudioHardwareUnknownPropertyError;
    }
    return noErr;
}

OSStatus DeviceDataSize(const Device& device, const AudioObjectPropertyAddress& address,
                        UInt32* size) {
    if (size == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    switch (address.mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
            *size = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyOwner:
        case kAudioDevicePropertyTransportType:
        case kAudioDevicePropertyClockDomain:
        case kAudioDevicePropertyDeviceIsAlive:
        case kAudioDevicePropertyDeviceIsRunning:
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
        case kAudioDevicePropertyLatency:
        case kAudioDevicePropertySafetyOffset:
        case kAudioDevicePropertyIsHidden:
        case kAudioDevicePropertyZeroTimeStampPeriod:
        case kAudioDevicePropertyClockIsStable:
        case kAudioDevicePropertyBufferFrameSize:
            *size = sizeof(UInt32);
            break;
        case kAudioObjectPropertyName:
        case kAudioObjectPropertyManufacturer:
        case kAudioDevicePropertyDeviceUID:
        case kAudioDevicePropertyModelUID:
            *size = sizeof(CFStringRef);
            break;
        case kAudioObjectPropertyOwnedObjects:
        case kAudioDevicePropertyStreams:
            *size = StreamCountForScope(address) * sizeof(AudioObjectID);
            break;
        case kAudioDevicePropertyRelatedDevices:
            *size = sizeof(AudioObjectID);
            break;
        case kAudioObjectPropertyControlList:
            *size = 0;
            break;
        case kAudioDevicePropertyNominalSampleRate:
            *size = sizeof(Float64);
            break;
        case kAudioDevicePropertyAvailableNominalSampleRates:
        case kAudioDevicePropertyBufferFrameSizeRange:
            *size = sizeof(AudioValueRange);
            break;
        case kAudioDevicePropertyPreferredChannelsForStereo:
            *size = 2 * sizeof(UInt32);
            break;
        case kAudioDevicePropertyStreamConfiguration:
            *size = offsetof(AudioBufferList, mBuffers) + sizeof(AudioBuffer);
            break;
        default:
            return kAudioHardwareUnknownPropertyError;
    }
    (void)device;
    return noErr;
}

OSStatus StreamDataSize(const AudioObjectPropertyAddress& address, UInt32* size) {
    if (size == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    switch (address.mSelector) {
        case kAudioObjectPropertyBaseClass:
        case kAudioObjectPropertyClass:
            *size = sizeof(AudioClassID);
            break;
        case kAudioObjectPropertyOwner:
        case kAudioStreamPropertyIsActive:
        case kAudioStreamPropertyDirection:
        case kAudioStreamPropertyTerminalType:
        case kAudioStreamPropertyStartingChannel:
        case kAudioStreamPropertyLatency:
            *size = sizeof(UInt32);
            break;
        case kAudioObjectPropertyOwnedObjects:
            *size = 0;
            break;
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat:
            *size = sizeof(AudioStreamBasicDescription);
            break;
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats:
            *size = sizeof(AudioStreamRangedDescription);
            break;
        default:
            return kAudioHardwareUnknownPropertyError;
    }
    return noErr;
}

OSStatus PlugInData(const AudioObjectPropertyAddress& address, UInt32 qualifierSize,
                    const void* qualifier, UInt32 inputSize, UInt32* outputSize, void* output) {
    std::lock_guard lock(gStateMutex);
    switch (address.mSelector) {
        case kAudioObjectPropertyBaseClass: {
            const AudioClassID value = kAudioObjectClassID;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioObjectPropertyClass: {
            const AudioClassID value = kAudioPlugInClassID;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioObjectPropertyOwner: {
            const AudioObjectID value = kAudioObjectUnknown;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioObjectPropertyManufacturer:
        case kAudioPlugInPropertyBundleID:
        case kAudioPlugInPropertyResourceBundle: {
            const char* text = address.mSelector == kAudioObjectPropertyManufacturer
                ? kManufacturer
                : (address.mSelector == kAudioPlugInPropertyBundleID ? kBundleID : "");
            CFStringRef value = CFStringCreateWithCString(kCFAllocatorDefault, text,
                                                          kCFStringEncodingUTF8);
            const OSStatus status = PutString(inputSize, outputSize, output, value);
            if (value != nullptr) {
                CFRelease(value);
            }
            return status;
        }
        case kAudioObjectPropertyOwnedObjects:
        case kAudioPlugInPropertyDeviceList: {
            const size_t count = std::min(gActiveDevices.size(),
                                          static_cast<size_t>(inputSize / sizeof(AudioObjectID)));
            auto* values = static_cast<AudioObjectID*>(output);
            for (size_t index = 0; index < count; ++index) {
                values[index] = gActiveDevices[index]->DeviceObjectID();
            }
            *outputSize = static_cast<UInt32>(count * sizeof(AudioObjectID));
            return noErr;
        }
        case kAudioObjectPropertyCustomPropertyInfoList: {
            const AudioServerPlugInCustomPropertyInfo values[] = {
                {kVCableControlProperty, kAudioServerPlugInCustomPropertyDataTypeCFString,
                 kAudioServerPlugInCustomPropertyDataTypeNone},
                {kVCableStatusProperty, kAudioServerPlugInCustomPropertyDataTypeCFString,
                 kAudioServerPlugInCustomPropertyDataTypeNone},
            };
            return PutBytes(inputSize, sizeof(values), outputSize, output, values);
        }
        case kAudioPlugInPropertyTranslateUIDToDevice: {
            if (qualifierSize != sizeof(CFStringRef) || qualifier == nullptr) {
                return kAudioHardwareBadPropertySizeError;
            }
            const CFStringRef uid = *static_cast<const CFStringRef*>(qualifier);
            const Device* device = FindByUIDLocked(uid);
            const AudioObjectID value = device == nullptr ? kAudioObjectUnknown
                                                          : device->DeviceObjectID();
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioPlugInPropertyBoxList:
            *outputSize = 0;
            return noErr;
        case kVCableControlProperty:
        case kVCableStatusProperty: {
            CFStringRef value = MakeString(InventoryLocked());
            const OSStatus status = PutString(inputSize, outputSize, output, value);
            if (value != nullptr) {
                CFRelease(value);
            }
            return status;
        }
        default:
            return kAudioHardwareUnknownPropertyError;
    }
}

OSStatus DeviceData(const Device& device, const AudioObjectPropertyAddress& address,
                    UInt32 inputSize, UInt32* outputSize, void* output) {
    const bool inputScope = address.mScope == kAudioObjectPropertyScopeInput;
    switch (address.mSelector) {
        case kAudioObjectPropertyBaseClass: {
            const AudioClassID value = kAudioObjectClassID;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioObjectPropertyClass: {
            const AudioClassID value = kAudioDeviceClassID;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioObjectPropertyOwner: {
            const AudioObjectID value = kAudioObjectPlugInObject;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioObjectPropertyName:
            return PutString(inputSize, outputSize, output, device.nameString);
        case kAudioObjectPropertyManufacturer: {
            CFStringRef value = CFStringCreateWithCString(kCFAllocatorDefault, kManufacturer,
                                                          kCFStringEncodingUTF8);
            const OSStatus status = PutString(inputSize, outputSize, output, value);
            if (value != nullptr) {
                CFRelease(value);
            }
            return status;
        }
        case kAudioDevicePropertyDeviceUID:
            return PutString(inputSize, outputSize, output, device.uidString);
        case kAudioDevicePropertyModelUID: {
            CFStringRef value = CFStringCreateWithCString(kCFAllocatorDefault, kModelUID,
                                                          kCFStringEncodingUTF8);
            const OSStatus status = PutString(inputSize, outputSize, output, value);
            if (value != nullptr) {
                CFRelease(value);
            }
            return status;
        }
        case kAudioObjectPropertyOwnedObjects:
        case kAudioDevicePropertyStreams: {
            AudioObjectID streams[2] = {device.InputStreamObjectID(),
                                        device.OutputStreamObjectID()};
            UInt32 count = 2;
            if (address.mScope == kAudioObjectPropertyScopeInput) {
                streams[0] = device.InputStreamObjectID();
                count = 1;
            } else if (address.mScope == kAudioObjectPropertyScopeOutput) {
                streams[0] = device.OutputStreamObjectID();
                count = 1;
            }
            return PutBytes(inputSize, count * sizeof(AudioObjectID), outputSize, output, streams);
        }
        case kAudioDevicePropertyTransportType: {
            const UInt32 value = kAudioDeviceTransportTypeVirtual;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioDevicePropertyRelatedDevices: {
            const AudioObjectID value = device.DeviceObjectID();
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioDevicePropertyClockDomain: {
            const UInt32 value = 1;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioDevicePropertyDeviceIsAlive:
        case kAudioDevicePropertyDeviceCanBeDefaultDevice:
        case kAudioDevicePropertyDeviceCanBeDefaultSystemDevice:
        case kAudioDevicePropertyClockIsStable: {
            const UInt32 value = 1;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioDevicePropertyDeviceIsRunning: {
            const UInt32 value = device.runningClients.load(std::memory_order_relaxed) > 0 ? 1 : 0;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioDevicePropertyLatency: {
            const UInt32 value = inputScope ? kLoopbackLatencyFrames : 0;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioDevicePropertySafetyOffset: {
            const UInt32 value = 0;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioObjectPropertyControlList:
            *outputSize = 0;
            return noErr;
        case kAudioDevicePropertyNominalSampleRate:
            return PutValue(inputSize, outputSize, output, device.sampleRate);
        case kAudioDevicePropertyAvailableNominalSampleRates: {
            const AudioValueRange value = {device.sampleRate, device.sampleRate};
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioDevicePropertyIsHidden: {
            const UInt32 value = 0;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioDevicePropertyPreferredChannelsForStereo: {
            const UInt32 availableChannels = ChannelsForScope(device, address.mScope);
            const UInt32 value[2] = {1, availableChannels >= 2 ? 2U : 1U};
            return PutBytes(inputSize, sizeof(value), outputSize, output, value);
        }
        case kAudioDevicePropertyStreamConfiguration: {
            const UInt32 required = offsetof(AudioBufferList, mBuffers) + sizeof(AudioBuffer);
            if (inputSize < required) {
                return kAudioHardwareBadPropertySizeError;
            }
            auto* list = static_cast<AudioBufferList*>(output);
            list->mNumberBuffers = 1;
            list->mBuffers[0].mNumberChannels = ChannelsForScope(device, address.mScope);
            list->mBuffers[0].mDataByteSize = 0;
            list->mBuffers[0].mData = nullptr;
            *outputSize = required;
            return noErr;
        }
        case kAudioDevicePropertyZeroTimeStampPeriod: {
            const UInt32 value = kZeroTimeStampPeriod;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioDevicePropertyBufferFrameSize: {
            const UInt32 value = kBufferFrameSize;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioDevicePropertyBufferFrameSizeRange: {
            const AudioValueRange value = {kBufferFrameSize, kBufferFrameSize};
            return PutValue(inputSize, outputSize, output, value);
        }
        default:
            return kAudioHardwareUnknownPropertyError;
    }
}

OSStatus StreamData(const Device& device, bool input, const AudioObjectPropertyAddress& address,
                    UInt32 inputSize, UInt32* outputSize, void* output) {
    switch (address.mSelector) {
        case kAudioObjectPropertyBaseClass: {
            const AudioClassID value = kAudioObjectClassID;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioObjectPropertyClass: {
            const AudioClassID value = kAudioStreamClassID;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioObjectPropertyOwner: {
            const AudioObjectID value = device.DeviceObjectID();
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioObjectPropertyOwnedObjects:
            *outputSize = 0;
            return noErr;
        case kAudioStreamPropertyIsActive: {
            const UInt32 value = (input ? device.inputActive : device.outputActive)
                                         .load(std::memory_order_relaxed)
                ? 1
                : 0;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioStreamPropertyDirection: {
            const UInt32 value = input ? 1 : 0;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioStreamPropertyTerminalType: {
            const UInt32 value = input ? kAudioStreamTerminalTypeMicrophone
                                       : kAudioStreamTerminalTypeSpeaker;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioStreamPropertyStartingChannel: {
            const UInt32 value = 1;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioStreamPropertyLatency: {
            const UInt32 value = input ? kLoopbackLatencyFrames : 0;
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioStreamPropertyVirtualFormat:
        case kAudioStreamPropertyPhysicalFormat: {
            const AudioStreamBasicDescription value = StreamFormat(device, input);
            return PutValue(inputSize, outputSize, output, value);
        }
        case kAudioStreamPropertyAvailableVirtualFormats:
        case kAudioStreamPropertyAvailablePhysicalFormats: {
            const AudioStreamRangedDescription value = {
                StreamFormat(device, input), {device.sampleRate, device.sampleRate}};
            return PutValue(inputSize, outputSize, output, value);
        }
        default:
            return kAudioHardwareUnknownPropertyError;
    }
}

HRESULT QueryInterface(void* driver, REFIID uuid, LPVOID* output) {
    if (driver != gDriverRef || output == nullptr) {
        return E_INVALIDARG;
    }
    CFUUIDRef requested = CFUUIDCreateFromUUIDBytes(kCFAllocatorDefault, uuid);
    if (requested == nullptr) {
        return E_INVALIDARG;
    }
    const bool supported = CFEqual(requested, IUnknownUUID) ||
                           CFEqual(requested, kAudioServerPlugInDriverInterfaceUUID);
    CFRelease(requested);
    if (!supported) {
        *output = nullptr;
        return E_NOINTERFACE;
    }
    AddRef(driver);
    *output = gDriverRef;
    return S_OK;
}

ULONG AddRef(void* driver) {
    if (driver != gDriverRef) {
        return 0;
    }
    UInt32 current = gReferenceCount.load(std::memory_order_relaxed);
    while (current != std::numeric_limits<UInt32>::max() &&
           !gReferenceCount.compare_exchange_weak(current, current + 1,
                                                  std::memory_order_relaxed)) {
    }
    return current == std::numeric_limits<UInt32>::max() ? current : current + 1;
}

ULONG Release(void* driver) {
    if (driver != gDriverRef) {
        return 0;
    }
    UInt32 current = gReferenceCount.load(std::memory_order_relaxed);
    while (current > 0 && !gReferenceCount.compare_exchange_weak(
                              current, current - 1, std::memory_order_relaxed)) {
    }
    return current > 0 ? current - 1 : 0;
}

OSStatus Initialize(AudioServerPlugInDriverRef driver, AudioServerPlugInHostRef host) {
    if (!IsDriver(driver) || host == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    std::lock_guard lock(gStateMutex);
    gHost = host;
    mach_timebase_info_data_t timebase{};
    mach_timebase_info(&timebase);
    gHostTicksPerSecond = 1'000'000'000.0 * static_cast<Float64>(timebase.denom) /
                          static_cast<Float64>(timebase.numer);

    CFStringRef key = CFStringCreateWithCString(kCFAllocatorDefault, kStorageKey,
                                                kCFStringEncodingUTF8);
    CFPropertyListRef stored = nullptr;
    const OSStatus copyStatus = host->CopyFromStorage(host, key, &stored);
    CFRelease(key);
    if (copyStatus != noErr) {
        return copyStatus;
    }
    if (stored == nullptr) {
        return noErr;
    }
    if (CFGetTypeID(stored) != CFStringGetTypeID()) {
        CFRelease(stored);
        return kAudioHardwareIllegalOperationError;
    }
    std::string inventory;
    const bool converted = ToString(static_cast<CFStringRef>(stored), inventory);
    CFRelease(stored);
    if (!converted) {
        return kAudioHardwareIllegalOperationError;
    }
    for (const std::string& line : Split(inventory, '\n')) {
        if (line.empty()) {
            continue;
        }
        const OSStatus status = AddDeviceLocked(Split(line, '\t'), false, nullptr);
        if (status != noErr) {
            return status;
        }
    }
    return noErr;
}

OSStatus CreateDevice(AudioServerPlugInDriverRef driver, CFDictionaryRef,
                      const AudioServerPlugInClientInfo*, AudioObjectID*) {
    return IsDriver(driver) ? kAudioHardwareUnsupportedOperationError
                            : kAudioHardwareBadObjectError;
}

OSStatus DestroyDevice(AudioServerPlugInDriverRef driver, AudioObjectID objectID) {
    if (!IsDriver(driver)) {
        return kAudioHardwareBadObjectError;
    }
    Device* device = DeviceForObject(objectID);
    if (device == nullptr || objectID != device->DeviceObjectID()) {
        return kAudioHardwareBadObjectError;
    }
    OSStatus status = noErr;
    {
        std::lock_guard lock(gStateMutex);
        status = RemoveDeviceLocked(device, true);
    }
    if (status == noErr) {
        NotifyDeviceListChanged();
        std::lock_guard lock(gStateMutex);
        ReleaseRetiredSlotLocked(device);
    }
    return status;
}

OSStatus AddDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID objectID,
                         const AudioServerPlugInClientInfo*) {
    Device* device = DeviceForObject(objectID);
    if (!IsDriver(driver) || device == nullptr || objectID != device->DeviceObjectID()) {
        return kAudioHardwareBadObjectError;
    }
    std::lock_guard lock(gStateMutex);
    if (gSlots[device->slot].load(std::memory_order_acquire) != device) {
        return kAudioHardwareBadObjectError;
    }
    device->attachedClients.fetch_add(1, std::memory_order_relaxed);
    return noErr;
}

OSStatus RemoveDeviceClient(AudioServerPlugInDriverRef driver, AudioObjectID objectID,
                            const AudioServerPlugInClientInfo*) {
    Device* device = DeviceForObject(objectID);
    if (!IsDriver(driver) || device == nullptr || objectID != device->DeviceObjectID()) {
        return kAudioHardwareBadObjectError;
    }
    std::lock_guard lock(gStateMutex);
    if (gSlots[device->slot].load(std::memory_order_acquire) != device) {
        return kAudioHardwareBadObjectError;
    }
    UInt32 current = device->attachedClients.load(std::memory_order_relaxed);
    while (current > 0 && !device->attachedClients.compare_exchange_weak(
                              current, current - 1, std::memory_order_relaxed)) {
    }
    if (current == 0) {
        return kAudioHardwareIllegalOperationError;
    }
    ReleaseRetiredSlotLocked(device);
    return noErr;
}

OSStatus PerformDeviceConfigurationChange(AudioServerPlugInDriverRef driver,
                                           AudioObjectID objectID, UInt64 action,
                                           void* changeInfo) {
    Device* device = DeviceForObject(objectID);
    if (!IsDriver(driver) || device == nullptr || objectID != device->DeviceObjectID() ||
        action != kDeleteDeviceConfigurationAction || changeInfo != device) {
        return kAudioHardwareBadObjectError;
    }
    OSStatus status = noErr;
    {
        std::lock_guard lock(gStateMutex);
        if (!device->deletePending.exchange(false, std::memory_order_acq_rel)) {
            return kAudioHardwareIllegalOperationError;
        }
        status = RemoveDeviceLocked(device, true);
    }
    if (status == noErr) {
        NotifyDeviceListChanged();
        std::lock_guard lock(gStateMutex);
        ReleaseRetiredSlotLocked(device);
    }
    return status;
}

OSStatus AbortDeviceConfigurationChange(AudioServerPlugInDriverRef driver,
                                         AudioObjectID objectID, UInt64 action,
                                         void* changeInfo) {
    Device* device = DeviceForObject(objectID);
    if (!IsDriver(driver) || device == nullptr || objectID != device->DeviceObjectID() ||
        action != kDeleteDeviceConfigurationAction || changeInfo != device) {
        return kAudioHardwareBadObjectError;
    }
    device->deletePending.store(false, std::memory_order_release);
    return noErr;
}

Boolean HasProperty(AudioServerPlugInDriverRef driver, AudioObjectID objectID, pid_t,
                    const AudioObjectPropertyAddress* address) {
    if (!IsDriver(driver) || address == nullptr) {
        return false;
    }
    switch (KindForObject(objectID)) {
        case ObjectKind::PlugIn:
            return PlugInHas(address->mSelector);
        case ObjectKind::Device:
            return DeviceHas(address->mSelector);
        case ObjectKind::InputStream:
        case ObjectKind::OutputStream:
            return StreamHas(address->mSelector);
        default:
            return false;
    }
}

OSStatus IsPropertySettable(AudioServerPlugInDriverRef driver, AudioObjectID objectID, pid_t,
                            const AudioObjectPropertyAddress* address, Boolean* settable) {
    if (!IsDriver(driver) || address == nullptr || settable == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    if (!HasProperty(driver, objectID, 0, address)) {
        return kAudioHardwareUnknownPropertyError;
    }
    const ObjectKind kind = KindForObject(objectID);
    *settable = (kind == ObjectKind::PlugIn && address->mSelector == kVCableControlProperty) ||
                ((kind == ObjectKind::InputStream || kind == ObjectKind::OutputStream) &&
                 address->mSelector == kAudioStreamPropertyIsActive);
    return noErr;
}

OSStatus GetPropertyDataSize(AudioServerPlugInDriverRef driver, AudioObjectID objectID, pid_t,
                             const AudioObjectPropertyAddress* address, UInt32, const void*,
                             UInt32* size) {
    if (!IsDriver(driver) || address == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    Device* device = DeviceForObject(objectID);
    switch (KindForObject(objectID)) {
        case ObjectKind::PlugIn:
            return PlugInDataSize(*address, size);
        case ObjectKind::Device:
            return DeviceDataSize(*device, *address, size);
        case ObjectKind::InputStream:
        case ObjectKind::OutputStream:
            return StreamDataSize(*address, size);
        default:
            return kAudioHardwareBadObjectError;
    }
}

OSStatus GetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID objectID, pid_t,
                         const AudioObjectPropertyAddress* address, UInt32 qualifierSize,
                         const void* qualifier, UInt32 inputSize, UInt32* outputSize, void* output) {
    if (!IsDriver(driver) || address == nullptr || outputSize == nullptr || output == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    Device* device = DeviceForObject(objectID);
    switch (KindForObject(objectID)) {
        case ObjectKind::PlugIn:
            return PlugInData(*address, qualifierSize, qualifier, inputSize, outputSize, output);
        case ObjectKind::Device:
            return DeviceData(*device, *address, inputSize, outputSize, output);
        case ObjectKind::InputStream:
            return StreamData(*device, true, *address, inputSize, outputSize, output);
        case ObjectKind::OutputStream:
            return StreamData(*device, false, *address, inputSize, outputSize, output);
        default:
            return kAudioHardwareBadObjectError;
    }
}

OSStatus SetPropertyData(AudioServerPlugInDriverRef driver, AudioObjectID objectID, pid_t,
                         const AudioObjectPropertyAddress* address, UInt32, const void*,
                         UInt32 inputSize, const void* input) {
    if (!IsDriver(driver) || address == nullptr || input == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    const ObjectKind kind = KindForObject(objectID);
    if (kind == ObjectKind::PlugIn && address->mSelector == kVCableControlProperty) {
        if (inputSize != sizeof(CFStringRef)) {
            return kAudioHardwareBadPropertySizeError;
        }
        std::string command;
        if (!ToString(*static_cast<const CFStringRef*>(input), command)) {
            return kAudioHardwareIllegalOperationError;
        }
        const std::vector<std::string> fields = Split(command, '\t');
        bool created = false;
        Device* deleteDevice = nullptr;
        OSStatus status = noErr;
        {
            std::lock_guard lock(gStateMutex);
            if (!fields.empty() && fields[0] == "create") {
                Device* existing = fields.size() > 1 ? FindByIDLocked(fields[1]) : nullptr;
                status = AddDeviceLocked(fields, true, nullptr);
                created = status == noErr && existing == nullptr;
            } else if (fields.size() == 2 && fields[0] == "delete") {
                deleteDevice = FindByIDLocked(fields[1]);
                if (deleteDevice == nullptr) {
                    status = kAudioHardwareBadObjectError;
                } else if (deleteDevice->runningClients.load(std::memory_order_acquire) != 0 ||
                           deleteDevice->deletePending.exchange(true,
                                                                std::memory_order_acq_rel)) {
                    status = kAudioHardwareIllegalOperationError;
                }
            } else {
                status = kAudioHardwareIllegalOperationError;
            }
        }
        if (created) {
            NotifyDeviceListChanged();
        } else if (status == noErr && deleteDevice != nullptr) {
            if (gHost == nullptr || gHost->RequestDeviceConfigurationChange == nullptr) {
                deleteDevice->deletePending.store(false, std::memory_order_release);
                return kAudioHardwareNotReadyError;
            }
            status = gHost->RequestDeviceConfigurationChange(
                gHost, deleteDevice->DeviceObjectID(), kDeleteDeviceConfigurationAction,
                deleteDevice);
            if (status != noErr) {
                deleteDevice->deletePending.store(false, std::memory_order_release);
            }
        }
        return status;
    }
    if ((kind == ObjectKind::InputStream || kind == ObjectKind::OutputStream) &&
        address->mSelector == kAudioStreamPropertyIsActive) {
        if (inputSize != sizeof(UInt32)) {
            return kAudioHardwareBadPropertySizeError;
        }
        Device* device = DeviceForObject(objectID);
        const bool active = *static_cast<const UInt32*>(input) != 0;
        (kind == ObjectKind::InputStream ? device->inputActive : device->outputActive)
            .store(active, std::memory_order_release);
        if (gHost != nullptr) {
            gHost->PropertiesChanged(gHost, objectID, 1, address);
        }
        return noErr;
    }
    return kAudioHardwareUnknownPropertyError;
}

OSStatus StartIO(AudioServerPlugInDriverRef driver, AudioObjectID objectID, UInt32) {
    Device* device = DeviceForObject(objectID);
    if (!IsDriver(driver) || device == nullptr || objectID != device->DeviceObjectID()) {
        return kAudioHardwareBadObjectError;
    }
    std::lock_guard lock(gStateMutex);
    if (!device->active.load(std::memory_order_acquire)) {
        return kAudioHardwareBadObjectError;
    }
    if (device->runningClients.fetch_add(1, std::memory_order_acq_rel) == 0) {
        device->ClearTags();
        device->anchorHostTime.store(mach_absolute_time(), std::memory_order_release);
        device->clockSeed.fetch_add(1, std::memory_order_relaxed);
    }
    return noErr;
}

OSStatus StopIO(AudioServerPlugInDriverRef driver, AudioObjectID objectID, UInt32) {
    Device* device = DeviceForObject(objectID);
    if (!IsDriver(driver) || device == nullptr || objectID != device->DeviceObjectID()) {
        return kAudioHardwareBadObjectError;
    }
    std::lock_guard lock(gStateMutex);
    UInt32 current = device->runningClients.load(std::memory_order_relaxed);
    while (current > 0 && !device->runningClients.compare_exchange_weak(
                              current, current - 1, std::memory_order_acq_rel)) {
    }
    if (current == 0) {
        return kAudioHardwareIllegalOperationError;
    }
    ReleaseRetiredSlotLocked(device);
    return noErr;
}

OSStatus GetZeroTimeStamp(AudioServerPlugInDriverRef driver, AudioObjectID objectID, UInt32,
                          Float64* sampleTime, UInt64* hostTime, UInt64* seed) {
    Device* device = DeviceForObject(objectID);
    if (!IsDriver(driver) || device == nullptr || sampleTime == nullptr || hostTime == nullptr ||
        seed == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    const UInt64 anchor = device->anchorHostTime.load(std::memory_order_acquire);
    if (anchor == 0 || gHostTicksPerSecond <= 0.0) {
        return kAudioHardwareNotRunningError;
    }
    const Float64 ticksPerPeriod = gHostTicksPerSecond / device->sampleRate *
                                   static_cast<Float64>(kZeroTimeStampPeriod);
    const UInt64 now = mach_absolute_time();
    const UInt64 period = now > anchor
        ? static_cast<UInt64>(static_cast<Float64>(now - anchor) / ticksPerPeriod)
        : 0;
    *sampleTime = static_cast<Float64>(period * kZeroTimeStampPeriod);
    *hostTime = anchor + static_cast<UInt64>(static_cast<Float64>(period) * ticksPerPeriod);
    *seed = device->clockSeed.load(std::memory_order_relaxed);
    return noErr;
}

OSStatus WillDoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID objectID, UInt32,
                           UInt32 operation, Boolean* willDo, Boolean* inPlace) {
    Device* device = DeviceForObject(objectID);
    if (!IsDriver(driver) || device == nullptr || willDo == nullptr || inPlace == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    *willDo = operation == kAudioServerPlugInIOOperationReadInput ||
              operation == kAudioServerPlugInIOOperationWriteMix;
    *inPlace = true;
    return noErr;
}

OSStatus BeginIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID objectID, UInt32,
                          UInt32, UInt32, const AudioServerPlugInIOCycleInfo*) {
    return IsDriver(driver) && DeviceForObject(objectID) != nullptr
        ? static_cast<OSStatus>(noErr)
        : static_cast<OSStatus>(kAudioHardwareBadObjectError);
}

OSStatus DoIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID objectID,
                       AudioObjectID streamID, UInt32, UInt32 operation, UInt32 frames,
                       const AudioServerPlugInIOCycleInfo* cycle, void* mainBuffer, void*) {
    Device* device = DeviceForObject(objectID);
    if (!IsDriver(driver) || device == nullptr || cycle == nullptr || mainBuffer == nullptr) {
        return kAudioHardwareIllegalOperationError;
    }
    if (operation == kAudioServerPlugInIOOperationWriteMix &&
        streamID == device->OutputStreamObjectID()) {
        const auto sampleTime = static_cast<UInt64>(std::max(0.0, cycle->mOutputTime.mSampleTime));
        device->Write(sampleTime, frames, static_cast<const Float32*>(mainBuffer));
        return noErr;
    }
    if (operation == kAudioServerPlugInIOOperationReadInput &&
        streamID == device->InputStreamObjectID()) {
        const auto sampleTime = static_cast<UInt64>(std::max(0.0, cycle->mInputTime.mSampleTime));
        device->Read(sampleTime, frames, static_cast<Float32*>(mainBuffer));
        return noErr;
    }
    return kAudioHardwareUnsupportedOperationError;
}

OSStatus EndIOOperation(AudioServerPlugInDriverRef driver, AudioObjectID objectID, UInt32, UInt32,
                        UInt32, const AudioServerPlugInIOCycleInfo*) {
    return IsDriver(driver) && DeviceForObject(objectID) != nullptr
        ? static_cast<OSStatus>(noErr)
        : static_cast<OSStatus>(kAudioHardwareBadObjectError);
}

}  // namespace

extern "C" __attribute__((visibility("default"))) void* VCableFactory(
    CFAllocatorRef, CFUUIDRef requestedType) {
    if (requestedType == nullptr || !CFEqual(requestedType, kAudioServerPlugInTypeUUID)) {
        return nullptr;
    }
    AddRef(gDriverRef);
    return gDriverRef;
}
