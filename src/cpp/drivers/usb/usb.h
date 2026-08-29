#pragma once

#include <include/types.h>

namespace Kernel
{
namespace Usb
{

/* ---- Device speeds (xHCI Port Speed / Slot Context Speed encoding) ---- */

enum Speed : u8
{
    SpeedInvalid = 0,
    SpeedFull = 1,      /* 12 Mb/s  */
    SpeedLow = 2,       /* 1.5 Mb/s */
    SpeedHigh = 3,      /* 480 Mb/s */
    SpeedSuper = 4,     /* 5 Gb/s   */
    SpeedSuperPlus = 5, /* 10 Gb/s  */
};

const char* SpeedToStr(u8 speed);

/* Default control-endpoint packet size before the device descriptor is
   readable. Low/Full speed must start at 8, High speed is fixed at 64 and
   SuperSpeed encodes 512. */
u16 DefaultMaxPacket0(u8 speed);

/* ---- Standard request types (bmRequestType) ---- */

static const u8 ReqDirIn = 0x80;
static const u8 ReqDirOut = 0x00;
static const u8 ReqTypeStandard = 0x00;
static const u8 ReqTypeClass = 0x20;
static const u8 ReqRecipDevice = 0x00;
static const u8 ReqRecipInterface = 0x01;
static const u8 ReqRecipEndpoint = 0x02;
static const u8 ReqRecipOther = 0x03;   /* a hub's downstream port */

/* ---- Standard requests (bRequest) ---- */

static const u8 ReqGetStatus = 0x00;
static const u8 ReqClearFeature = 0x01;
static const u8 ReqSetFeature = 0x03;
static const u8 ReqSetAddress = 0x05;
static const u8 ReqGetDescriptor = 0x06;
static const u8 ReqGetConfiguration = 0x08;
static const u8 ReqSetConfiguration = 0x09;
static const u8 ReqSetInterface = 0x0B;

/* Endpoint feature selector used to clear a halt after a STALL */
static const u16 FeatureEndpointHalt = 0x0000;

/* ---- HID class requests ---- */

static const u8 HidReqGetReport = 0x01;
static const u8 HidReqSetIdle = 0x0A;
static const u8 HidReqSetProtocol = 0x0B;

static const u16 HidProtocolBoot = 0;

/* ---- Descriptor types (high byte of wValue in GET_DESCRIPTOR) ---- */

static const u8 DescDevice = 1;
static const u8 DescConfiguration = 2;
static const u8 DescString = 3;
static const u8 DescInterface = 4;
static const u8 DescEndpoint = 5;
static const u8 DescHid = 0x21;
static const u8 DescHub = 0x29;
static const u8 DescHubSuperSpeed = 0x2A;

/* ---- Class / subclass / protocol codes we care about ---- */

static const u8 ClassHid = 0x03;
static const u8 ClassHub = 0x09;

/* ---- Hub class port features and status bits (USB 2.0 spec 11.24) ---- */

static const u16 HubFeaturePortConnection = 0;
static const u16 HubFeaturePortEnable = 1;
static const u16 HubFeaturePortReset = 4;
static const u16 HubFeaturePortPower = 8;
static const u16 HubFeatureCPortConnection = 16;
static const u16 HubFeatureCPortEnable = 17;
static const u16 HubFeatureCPortReset = 20;
static const u16 HubFeatureBhPortReset = 28;
static const u16 HubFeatureCBhPortReset = 29;

static const u16 HubPortStatusConnection = 1u << 0;
static const u16 HubPortStatusEnable = 1u << 1;
static const u16 HubPortStatusReset = 1u << 4;
static const u16 HubPortStatusPower = 1u << 8;
static const u16 HubPortStatusLowSpeed = 1u << 9;
static const u16 HubPortStatusHighSpeed = 1u << 10;

static const u16 HubPortChangeConnection = 1u << 0;
static const u16 HubPortChangeReset = 1u << 4;

/* Offsets inside the hub class descriptor, identical for 0x29 and 0x2A up
   to bPwrOn2PwrGood. */
static const u32 HubDescNumPorts = 2;
static const u32 HubDescCharacteristics = 3;
static const u32 HubDescPowerOnDelay = 5;
static const u32 HubDescMinLength = 7;
static const u8 SubClassBoot = 0x01;
static const u8 ProtocolKeyboard = 0x01;

/* ---- Descriptor layouts (little endian, packed on the wire) ---- */

struct DeviceDescriptor
{
    u8 Length;
    u8 DescriptorType;
    u16 UsbVersion;
    u8 DeviceClass;
    u8 DeviceSubClass;
    u8 DeviceProtocol;
    u8 MaxPacketSize0;
    u16 VendorId;
    u16 ProductId;
    u16 DeviceVersion;
    u8 Manufacturer;
    u8 Product;
    u8 SerialNumber;
    u8 NumConfigurations;
} __attribute__((packed));

static_assert(sizeof(DeviceDescriptor) == 18, "bad device descriptor size");

struct ConfigDescriptor
{
    u8 Length;
    u8 DescriptorType;
    u16 TotalLength;
    u8 NumInterfaces;
    u8 ConfigurationValue;
    u8 Configuration;
    u8 Attributes;
    u8 MaxPower;
} __attribute__((packed));

static_assert(sizeof(ConfigDescriptor) == 9, "bad config descriptor size");

struct InterfaceDescriptor
{
    u8 Length;
    u8 DescriptorType;
    u8 InterfaceNumber;
    u8 AlternateSetting;
    u8 NumEndpoints;
    u8 InterfaceClass;
    u8 InterfaceSubClass;
    u8 InterfaceProtocol;
    u8 Interface;
} __attribute__((packed));

static_assert(sizeof(InterfaceDescriptor) == 9, "bad interface descriptor size");

struct EndpointDescriptor
{
    u8 Length;
    u8 DescriptorType;
    u8 EndpointAddress;
    u8 Attributes;
    u16 MaxPacketSize;
    u8 Interval;
} __attribute__((packed));

static_assert(sizeof(EndpointDescriptor) == 7, "bad endpoint descriptor size");

static const u8 EndpointDirIn = 0x80;
static const u8 EndpointNumMask = 0x0F;
static const u8 EndpointXferMask = 0x03;
static const u8 EndpointXferInterrupt = 0x03;

}
}
