// TTBlackholeUserClient.cpp — see TTBlackholeUserClient.iig. Selector contract lives in
// Shared/TTBlackholeABI.h; keep the two in step.
//
// Adapted from tinygrad's TinyGPUDriverUserClient.cpp (MIT).

#include <os/log.h>
#include <DriverKit/IOLib.h>
#include <DriverKit/IOUserClient.h>
#include <DriverKit/OSSharedPtr.h>
#include <DriverKit/OSData.h>
#include <DriverKit/IODMACommand.h>
#include <PCIDriverKit/PCIDriverKit.h>

#include "TTBlackholeUserClient.h"
#include "TTBlackholeDriver.h"
#include "../Shared/TTBlackholeABI.h"

struct TTBlackholeUserClient_IVars
{
    OSSharedPtr<TTBlackholeDriver> driver;
    // Live DMA mappings, indexed by the dma id handed to the client. nullptr = free slot.
    IODMACommand * dma[TTBH_MAX_DMA_MAPPINGS] = {};
};

// Unmap and drop one DMA mapping. Safe on an empty slot.
static void completeDMA(IODMACommand *& slot)
{
    if (slot == nullptr) return;
    slot->CompleteDMA(kIODMACommandCompleteDMANoOptions);
    slot->release();
    slot = nullptr;
}

bool TTBlackholeUserClient::init()
{
    if (!super::init()) return false;
    ivars = IONewZero(TTBlackholeUserClient_IVars, 1);
    return ivars != nullptr;
}

void TTBlackholeUserClient::free()
{
    if (ivars != nullptr) ivars->driver.reset();
    IOSafeDeleteNULL(ivars, TTBlackholeUserClient_IVars, 1);
    super::free();
}

kern_return_t IMPL(TTBlackholeUserClient, Start)
{
    kern_return_t err = Start(provider, SUPERDISPATCH);
    if (err != kIOReturnSuccess) return err;

    TTBlackholeDriver * driver = OSDynamicCast(TTBlackholeDriver, provider);
    if (driver == nullptr) return kIOReturnBadArgument;
    ivars->driver = OSSharedPtr(driver, OSRetain);
    return kIOReturnSuccess;
}

kern_return_t IMPL(TTBlackholeUserClient, Stop)
{
    // A client that exits (or crashes) without CompleteDMA must not leave DART mappings behind.
    for (auto & slot : ivars->dma) completeDMA(slot);
    ivars->driver.reset();
    return Stop(provider, SUPERDISPATCH);
}

kern_return_t TTBlackholeUserClient::ExternalMethod(uint64_t selector,
                                                    IOUserClientMethodArguments * args,
                                                    const IOUserClientMethodDispatch * dispatch,
                                                    OSObject * target,
                                                    void * reference)
{
    TTBlackholeDriver * driver = ivars->driver.get();
    if (driver == nullptr) return kIOReturnNotAttached;

    switch (selector) {
    case kTTBHGetInfo: {
        if (args->scalarOutputCount < kTTBHInfoCount) return kIOReturnBadArgument;
        uint32_t vendor = 0, device = 0, subVendor = 0, subId = 0;
        driver->CfgRead(kIOPCIConfigurationOffsetVendorID, 2, &vendor);
        driver->CfgRead(kIOPCIConfigurationOffsetDeviceID, 2, &device);
        driver->CfgRead(kIOPCIConfigurationOffsetSubSystemVendorID, 2, &subVendor);
        driver->CfgRead(kIOPCIConfigurationOffsetSubSystemID, 2, &subId);

        args->scalarOutput[kTTBHInfoABIVersion]     = TTBH_ABI_VERSION;
        args->scalarOutput[kTTBHInfoVendorID]       = vendor;
        args->scalarOutput[kTTBHInfoDeviceID]       = device;
        args->scalarOutput[kTTBHInfoSubsysVendorID] = subVendor;
        args->scalarOutput[kTTBHInfoSubsysID]       = subId;
        args->scalarOutput[kTTBHInfoBar0Size]       = driver->BarSize(0);
        args->scalarOutput[kTTBHInfoBar2Size]       = driver->BarSize(2);
        args->scalarOutput[kTTBHInfoBar4Size]       = driver->BarSize(4);
        args->scalarOutputCount = kTTBHInfoCount;
        return kIOReturnSuccess;
    }

    case kTTBHCfgRead: {
        if (args->scalarInputCount != 2 || args->scalarOutputCount < 1) return kIOReturnBadArgument;
        uint32_t value = 0;
        kern_return_t err = driver->CfgRead(uint32_t(args->scalarInput[0]),
                                            uint32_t(args->scalarInput[1]), &value);
        if (err != kIOReturnSuccess) return err;
        args->scalarOutput[0] = value;
        args->scalarOutputCount = 1;
        return kIOReturnSuccess;
    }

    case kTTBHPrepareDMA: {
        // A >4 KiB structure input arrives as a memory descriptor over the client's own pages.
        if (args->structureInputDescriptor == nullptr || args->scalarOutputCount < 2) return kIOReturnBadArgument;
        uint32_t id = 0;
        while (id < TTBH_MAX_DMA_MAPPINGS && ivars->dma[id] != nullptr) id++;
        if (id == TTBH_MAX_DMA_MAPPINGS) return kIOReturnNoResources;

        uint64_t length = 0;
        args->structureInputDescriptor->GetLength(&length);
        IOAddressSegment segs[TTBH_MAX_DMA_SEGMENTS] = {};
        uint32_t count = TTBH_MAX_DMA_SEGMENTS;
        IODMACommand * cmd = nullptr;
        kern_return_t err = driver->PrepareDMA(args->structureInputDescriptor, length, &cmd, segs, &count);
        if (err != kIOReturnSuccess) return err;

        TTBHDMASegment out[TTBH_MAX_DMA_SEGMENTS] = {};
        for (uint32_t i = 0; i < count; i++) out[i] = { segs[i].address, segs[i].length };
        args->structureOutput = OSData::withBytes(out, count * sizeof(TTBHDMASegment));
        if (args->structureOutput == nullptr) {
            cmd->CompleteDMA(kIODMACommandCompleteDMANoOptions);
            cmd->release();
            return kIOReturnNoMemory;
        }
        ivars->dma[id] = cmd;
        args->scalarOutput[0] = id;
        args->scalarOutput[1] = count;
        args->scalarOutputCount = 2;
        return kIOReturnSuccess;
    }

    case kTTBHCompleteDMA: {
        if (args->scalarInputCount != 1 || args->scalarInput[0] >= TTBH_MAX_DMA_MAPPINGS) return kIOReturnBadArgument;
        if (ivars->dma[args->scalarInput[0]] == nullptr) return kIOReturnNotFound;
        completeDMA(ivars->dma[args->scalarInput[0]]);
        return kIOReturnSuccess;
    }

    default:
        return kIOReturnUnsupported;
    }
}

kern_return_t IMPL(TTBlackholeUserClient, CopyClientMemoryForType)
{
    TTBlackholeDriver * driver = ivars->driver.get();
    if (driver == nullptr) return kIOReturnNotAttached;
    if (memory == nullptr) return kIOReturnBadArgument;

    switch (type) {
    case kTTBHMemoryBar0:
    case kTTBHMemoryBar2:
    case kTTBHMemoryBar4:
        os_log(OS_LOG_DEFAULT, "ttbh: mapping BAR%llu", type);
        return driver->CopyBarMemory(uint8_t(type), memory);
    default:
        return kIOReturnBadArgument;
    }
}
