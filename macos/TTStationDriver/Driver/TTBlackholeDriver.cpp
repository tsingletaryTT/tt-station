// TTBlackholeDriver.cpp — see TTBlackholeDriver.iig for the role of this class.
//
// Adapted from tinygrad's TinyGPUDriver.cpp (MIT). Differences from TinyGPU:
//   * matches only Tenstorrent Blackhole (Info.plist), not every display-class device;
//   * enables MEMORY space only — bus mastering stays off until DMA exists (spec M3), so the
//     chip cannot write host memory while the driver has no way to hand it a safe buffer;
//   * no reset / config-write / DMA paths yet (spec "User-client contract", v1).
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
    // "TTBlackholeUserClientProperties" is a dictionary in Info.plist naming the class to create.
    IOService * service = nullptr;
    kern_return_t err = Create(this, "TTBlackholeUserClientProperties", &service);
    if (err != kIOReturnSuccess) {
        os_log(OS_LOG_DEFAULT, "ttbh: user client Create failed 0x%08x", err);
        return err;
    }
    *userClient = OSDynamicCast(IOUserClient, service);
    if (*userClient == nullptr) {
        service->release();
        return kIOReturnError;
    }
    return kIOReturnSuccess;
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
