// TTBlackholeDriver.cpp — see TTBlackholeDriver.iig for the role of this class.
//
// Adapted from tinygrad's TinyGPUDriver.cpp (MIT). Differences from TinyGPU:
//   * matches only Tenstorrent Blackhole (Info.plist), not every display-class device;
//   * enables MEMORY space at Start; bus mastering only at the first PrepareDMA (spec M3), so the
//     chip cannot write host memory until a client has handed it a buffer that is actually mapped;
//   * DMA mapping (PrepareDMA) but no reset or config-write paths (spec "User-client contract");
//   * ONE user client at a time (NewUserClient refuses a second), because a client owns the BARs,
//     the TLB windows and the iATU outright, and two of them would silently clobber each other;
//   * on that client's exit, DMA is quiesced (iATU regions + bus mastering off) before its DART
//     mappings go, so a chip still mid-transfer can't write into pages that have been handed back.
//
// Logging goes to the unified log under the "ttbh:" prefix:
//   log stream --predicate 'eventMessage CONTAINS "ttbh:"'

#include <os/log.h>
#include <DriverKit/IOLib.h>
#include <DriverKit/IOUserClient.h>
#include <PCIDriverKit/PCIDriverKit.h>

#include "TTBlackholeDriver.h"
#include "../Shared/TTBlackholeABI.h"

struct TTBlackholeDriver_IVars
{
    IOPCIDevice * pci = nullptr;
    bool busMaster = false;   // turned on lazily by the first PrepareDMA
    bool clientOpen = false;  // exclusive user client; atomics because NewUserClient and the
                              // client's Stop can run on different dispatch queues
};

bool TTBlackholeDriver::init()
{
    if (!super::init()) return false;
    ivars = IONewZero(TTBlackholeDriver_IVars, 1);
    return ivars != nullptr;
}

void TTBlackholeDriver::free()
{
    IOSafeDeleteNULL(ivars, TTBlackholeDriver_IVars, 1);
    super::free();
}

kern_return_t IMPL(TTBlackholeDriver, Start)
{
    kern_return_t err = Start(provider, SUPERDISPATCH);
    if (err != kIOReturnSuccess) return err;

    ivars->pci = OSDynamicCast(IOPCIDevice, provider);
    if (ivars->pci == nullptr) {
        os_log(OS_LOG_DEFAULT, "ttbh: provider is not an IOPCIDevice");
        return kIOReturnNoDevice;
    }

    // Exclusive open: nothing else may drive this function while we hold it.
    err = ivars->pci->Open(this, 0);
    if (err != kIOReturnSuccess) {
        os_log(OS_LOG_DEFAULT, "ttbh: IOPCIDevice::Open failed 0x%08x", err);
        ivars->pci = nullptr;
        return err;
    }

    uint16_t vendor = 0, device = 0;
    ivars->pci->ConfigurationRead16(kIOPCIConfigurationOffsetVendorID, &vendor);
    ivars->pci->ConfigurationRead16(kIOPCIConfigurationOffsetDeviceID, &device);
    os_log(OS_LOG_DEFAULT, "ttbh: opened %04x:%04x", vendor, device);

    // Belt and braces: Info.plist already matches 1e52:b140, but if the personality is ever
    // loosened we must not start poking an arbitrary device's BARs.
    if (vendor != TTBH_PCI_VENDOR || device != TTBH_PCI_DEVICE) {
        os_log(OS_LOG_DEFAULT, "ttbh: unexpected device, refusing");
        ivars->pci->Close(this, 0);
        ivars->pci = nullptr;
        return kIOReturnUnsupported;
    }

    // Decode MMIO so the BAR mappings actually reach the chip. Bus mastering deliberately
    // left untouched (see file header).
    uint16_t command = 0;
    ivars->pci->ConfigurationRead16(kIOPCIConfigurationOffsetCommand, &command);
    ivars->pci->ConfigurationWrite16(kIOPCIConfigurationOffsetCommand,
                                     command | kIOPCICommandMemorySpace);

    // (Plain array: the DriverKit libc++ subset has no std::initializer_list.)
    static const uint8_t kBars[] = {0, 2, 4};
    for (uint8_t bar : kBars) {
        os_log(OS_LOG_DEFAULT, "ttbh: BAR%u size 0x%llx", bar, BarSize(bar));
    }

    // Name the service so userspace can find it with IOServiceNameMatching.
    SetName(TTBH_SERVICE_NAME);
    err = RegisterService();
    if (err != kIOReturnSuccess) {
        os_log(OS_LOG_DEFAULT, "ttbh: RegisterService failed 0x%08x", err);
        return err;
    }
    os_log(OS_LOG_DEFAULT, "ttbh: started, registered as %s", TTBH_SERVICE_NAME);
    return kIOReturnSuccess;
}

kern_return_t IMPL(TTBlackholeDriver, Stop)
{
    // Also reached on hot-unplug of the enclosure (spec risk R6).
    os_log(OS_LOG_DEFAULT, "ttbh: stopping");
    if (ivars->pci != nullptr) {
        ivars->pci->Close(this, 0);
        ivars->pci = nullptr;
    }
    return Stop(provider, SUPERDISPATCH);
}

kern_return_t IMPL(TTBlackholeDriver, NewUserClient)
{
    // Exclusive: claim the slot first, give it back if creation fails.
    if (__atomic_exchange_n(&ivars->clientOpen, true, __ATOMIC_ACQ_REL)) {
        os_log(OS_LOG_DEFAULT, "ttbh: refusing a second user client (one owner at a time)");
        return kIOReturnExclusiveAccess;
    }
    // "TTBlackholeUserClientProperties" is a dictionary in Info.plist naming the class to create.
    IOService * service = nullptr;
    kern_return_t err = Create(this, "TTBlackholeUserClientProperties", &service);
    if (err != kIOReturnSuccess) {
        os_log(OS_LOG_DEFAULT, "ttbh: user client Create failed 0x%08x", err);
        __atomic_store_n(&ivars->clientOpen, false, __ATOMIC_RELEASE);
        return err;
    }
    *userClient = OSDynamicCast(IOUserClient, service);
    if (*userClient == nullptr) {
        service->release();
        __atomic_store_n(&ivars->clientOpen, false, __ATOMIC_RELEASE);
        return kIOReturnError;
    }
    return kIOReturnSuccess;
}

void TTBlackholeDriver::ReleaseClient()
{
    if (ivars->pci != nullptr) {
        // Every outbound iATU region off: no chip NOC address maps to host memory any more. The
        // client programmed these through its BAR2 mapping; we only ever clear them.
        uint8_t memoryIndex = 0, barType = 0;
        uint64_t size = 0;
        if (ivars->pci->GetBARInfo(2, &memoryIndex, &size, &barType) == kIOReturnSuccess &&
            size >= TTBH_BAR2_IATU_BASE + TTBH_BAR2_IATU_REGIONS * TTBH_BAR2_IATU_STRIDE) {
            for (uint32_t r = 0; r < TTBH_BAR2_IATU_REGIONS; r++)
                ivars->pci->MemoryWrite32(memoryIndex, TTBH_BAR2_IATU_BASE + r * TTBH_BAR2_IATU_STRIDE + TTBH_BAR2_IATU_CTRL_2, 0);
        }
        // And bus mastering off, so even a region we couldn't clear can't reach memory. The next
        // client's first PrepareDMA turns it back on.
        if (ivars->busMaster) {
            uint16_t command16 = 0;
            ivars->pci->ConfigurationRead16(kIOPCIConfigurationOffsetCommand, &command16);
            ivars->pci->ConfigurationWrite16(kIOPCIConfigurationOffsetCommand,
                                             command16 & ~kIOPCICommandBusMaster);
            os_log(OS_LOG_DEFAULT, "ttbh: client gone: iATU cleared, bus mastering disabled");
        }
    }
    ivars->busMaster = false;
    __atomic_store_n(&ivars->clientOpen, false, __ATOMIC_RELEASE);
}

uint64_t TTBlackholeDriver::BarSize(uint8_t bar)
{
    if (ivars->pci == nullptr) return 0;
    uint8_t memoryIndex = 0, barType = 0;
    uint64_t size = 0;
    if (ivars->pci->GetBARInfo(bar, &memoryIndex, &size, &barType) != kIOReturnSuccess) return 0;
    return size;
}

kern_return_t TTBlackholeDriver::CopyBarMemory(uint8_t bar, IOMemoryDescriptor ** memory)
{
    if (ivars->pci == nullptr) return kIOReturnNotReady;
    uint8_t memoryIndex = 0, barType = 0;
    uint64_t size = 0;
    kern_return_t err = ivars->pci->GetBARInfo(bar, &memoryIndex, &size, &barType);
    if (err != kIOReturnSuccess) return err;
    // GetBARInfo translates a BAR number into the device's memory-range index, which is what
    // the copy call wants (the two differ: 64-bit BARs consume two BAR slots).
    return ivars->pci->_CopyDeviceMemoryWithIndex(memoryIndex, memory, this);
}

kern_return_t TTBlackholeDriver::CfgRead(uint32_t offset, uint32_t width, uint32_t * value)
{
    if (ivars->pci == nullptr || value == nullptr) return kIOReturnNotReady;
    // Extended config space is 4 KiB; the access must fit and be naturally aligned.
    if (offset >= 4096 || offset + width > 4096 || (offset % width) != 0) return kIOReturnBadArgument;

    switch (width) {
    case 1: { uint8_t v = 0;  ivars->pci->ConfigurationRead8(offset, &v);  *value = v; break; }
    case 2: { uint16_t v = 0; ivars->pci->ConfigurationRead16(offset, &v); *value = v; break; }
    case 4: { uint32_t v = 0; ivars->pci->ConfigurationRead32(offset, &v); *value = v; break; }
    default: return kIOReturnBadArgument;
    }
    return kIOReturnSuccess;
}

kern_return_t TTBlackholeDriver::PrepareDMA(IOMemoryDescriptor * memory, uint64_t length, IODMACommand ** command,
                                            IOAddressSegment * segments, uint32_t * segmentCount)
{
    if (ivars->pci == nullptr) return kIOReturnNotReady;
    if (memory == nullptr || command == nullptr || segments == nullptr || segmentCount == nullptr) return kIOReturnBadArgument;

    // 64 address bits: the iATU target is a full 64-bit address, so accept any IOVA DART hands out.
    IODMACommandSpecification spec = {};
    spec.options = 0;
    spec.maxAddressBits = 64;
    IODMACommand * cmd = nullptr;
    kern_return_t err = IODMACommand::Create(ivars->pci, kIODMACommandCreateNoOptions, &spec, &cmd);
    if (err != kIOReturnSuccess) {
        os_log(OS_LOG_DEFAULT, "ttbh: IODMACommand::Create failed 0x%08x", err);
        return err;
    }
    uint64_t flags = kIOMemoryDirectionInOut;
    err = cmd->PrepareForDMA(kIODMACommandPrepareForDMANoOptions, memory, 0, length, &flags, segmentCount, segments);
    if (err != kIOReturnSuccess) {
        os_log(OS_LOG_DEFAULT, "ttbh: PrepareForDMA failed 0x%08x", err);
        cmd->release();
        return err;
    }

    if (!ivars->busMaster) {
        uint16_t command16 = 0;
        ivars->pci->ConfigurationRead16(kIOPCIConfigurationOffsetCommand, &command16);
        ivars->pci->ConfigurationWrite16(kIOPCIConfigurationOffsetCommand, command16 | kIOPCICommandBusMaster);
        ivars->busMaster = true;
        os_log(OS_LOG_DEFAULT, "ttbh: bus mastering enabled (first DMA mapping)");
    }
    os_log(OS_LOG_DEFAULT, "ttbh: PrepareDMA %llu bytes -> %u segment(s)", length, *segmentCount);
    *command = cmd;
    return kIOReturnSuccess;
}
