// macOS virtual display for filling in when the Mac has no usable display (lid closed, no monitor).
// CoreGraphics has private CGVirtualDisplay* classes (what Sidecar and AirPlay displays are built on; DeskPad,
// BetterDisplay and Chromium's tests use them too). They have no public header, so everything is looked up at run
// time and checked before use: if a macOS release changes them we report an error instead of crashing.
#import <CoreGraphics/CoreGraphics.h>
#import <Foundation/Foundation.h>
#include <stdio.h>

@interface CGVirtualDisplayDescriptor : NSObject
@property(retain, nonatomic) dispatch_queue_t queue;
@property(retain, nonatomic) NSString *name;
@property(nonatomic) unsigned int maxPixelsWide;
@property(nonatomic) unsigned int maxPixelsHigh;
@property(nonatomic) CGSize sizeInMillimeters;
@property(nonatomic) unsigned int productID;
@property(nonatomic) unsigned int vendorID;
@property(nonatomic) unsigned int serialNum;
@end

@interface CGVirtualDisplayMode : NSObject
- (instancetype)initWithWidth:(unsigned int)width height:(unsigned int)height refreshRate:(double)refreshRate;
@end

@interface CGVirtualDisplaySettings : NSObject
@property(retain, nonatomic) NSArray *modes;
@property(nonatomic) unsigned int hiDPI;
@end

@interface CGVirtualDisplay : NSObject
- (instancetype)initWithDescriptor:(CGVirtualDisplayDescriptor *)descriptor;
- (BOOL)applySettings:(CGVirtualDisplaySettings *)settings;
@property(readonly, nonatomic) unsigned int displayID;
@end

static Class descriptorClass(void) { return NSClassFromString(@"CGVirtualDisplayDescriptor"); }
static Class modeClass(void) { return NSClassFromString(@"CGVirtualDisplayMode"); }
static Class settingsClass(void) { return NSClassFromString(@"CGVirtualDisplaySettings"); }
static Class displayClass(void) { return NSClassFromString(@"CGVirtualDisplay"); }

static void *failure(char *error, size_t size, NSString *what) {
    snprintf(error, size, "%s", what.UTF8String);
    return NULL;
}

int ah_vd_supported(void) {
    Class d = descriptorClass(), m = modeClass(), s = settingsClass(), v = displayClass();
    return d && m && s && v &&
           [d instancesRespondToSelector:@selector(setQueue:)] &&
           [d instancesRespondToSelector:@selector(setMaxPixelsWide:)] &&
           [d instancesRespondToSelector:@selector(setMaxPixelsHigh:)] &&
           [d instancesRespondToSelector:@selector(setSizeInMillimeters:)] &&
           [m instancesRespondToSelector:@selector(initWithWidth:height:refreshRate:)] &&
           [s instancesRespondToSelector:@selector(setModes:)] &&
           [s instancesRespondToSelector:@selector(setHiDPI:)] &&
           [v instancesRespondToSelector:@selector(initWithDescriptor:)] &&
           [v instancesRespondToSelector:@selector(applySettings:)] &&
           [v instancesRespondToSelector:@selector(displayID)];
}

// One mode, `width` x `height`: points when `hidpi` (the frame buffer is twice as large), pixels otherwise.
// Returns the display (retained: it exists until ah_vd_destroy) and its CGDirectDisplayID in `display`.
void *ah_vd_create(uint32_t width, uint32_t height, int hidpi, double hz, double mm_w, double mm_h,
                   uint32_t vendor, uint32_t product, uint32_t serial, const char *name,
                   uint32_t *display, char *error, size_t size) {
    @autoreleasepool {
        *display = kCGNullDirectDisplay;
        if (!ah_vd_supported()) return failure(error, size, @"this macOS has no usable CGVirtualDisplay");
        @try {
            static dispatch_queue_t queue;
            static dispatch_once_t once;
            dispatch_once(&once, ^{
                queue = dispatch_queue_create("app.airhand.virtual-display", DISPATCH_QUEUE_SERIAL);
            });
            uint32_t scale = hidpi ? 2 : 1;
            CGVirtualDisplayDescriptor *descriptor = [[descriptorClass() alloc] init];
            descriptor.queue = queue;
            if ([descriptor respondsToSelector:@selector(setName:)]) descriptor.name = @(name);
            descriptor.maxPixelsWide = width * scale;
            descriptor.maxPixelsHigh = height * scale;
            // The physical size decides how large things look (the default "looks like" size)
            descriptor.sizeInMillimeters = CGSizeMake(mm_w, mm_h);
            // Fixed identity: macOS remembers the arrangement and the windows' places per display
            if ([descriptor respondsToSelector:@selector(setVendorID:)]) descriptor.vendorID = vendor;
            if ([descriptor respondsToSelector:@selector(setProductID:)]) descriptor.productID = product;
            if ([descriptor respondsToSelector:@selector(setSerialNum:)]) descriptor.serialNum = serial;

            CGVirtualDisplay *virtualDisplay = [[displayClass() alloc] initWithDescriptor:descriptor];
            if (!virtualDisplay) return failure(error, size, @"CGVirtualDisplay refused the descriptor");
            CGVirtualDisplayMode *mode = [[modeClass() alloc] initWithWidth:width height:height refreshRate:hz];
            if (!mode) return failure(error, size, @"CGVirtualDisplayMode refused the mode");
            CGVirtualDisplaySettings *settings = [[settingsClass() alloc] init];
            settings.modes = @[mode];
            settings.hiDPI = hidpi ? 1 : 0;
            if (![virtualDisplay applySettings:settings])
                return failure(error, size, @"CGVirtualDisplay refused the settings");
            *display = virtualDisplay.displayID;
            return (__bridge_retained void *)virtualDisplay;
        } @catch (NSException *e) {
            return failure(error, size, [NSString stringWithFormat:@"CGVirtualDisplay: %@", e.reason]);
        }
    }
}

// The display goes away once nothing holds it.
void ah_vd_destroy(void *display) {
    @autoreleasepool {
        (void)CFBridgingRelease(display);
    }
}

// The current mode of `display`: its size in points and in pixels (`out`: width, height, pixel width, pixel height).
// Returns 0 if it can't be read. The process that created a virtual display can't read (or set) its mode, other
// processes can (see mac.rs).
int ah_vd_mode(uint32_t display, uint32_t out[4]) {
    CGDisplayModeRef mode = CGDisplayCopyDisplayMode(display);
    if (!mode) return 0;
    out[0] = (uint32_t)CGDisplayModeGetWidth(mode);
    out[1] = (uint32_t)CGDisplayModeGetHeight(mode);
    out[2] = (uint32_t)CGDisplayModeGetPixelWidth(mode);
    out[3] = (uint32_t)CGDisplayModeGetPixelHeight(mode);
    CGDisplayModeRelease(mode);
    return 1;
}

// Switch `display` to its mode of `width` x `height` points and `pixel_width` x `pixel_height` pixels (the low
// resolution twins of HiDPI modes included), remembered by macOS: the next time this display is in the same set of
// displays, macOS picks this mode again. Returns a CGError, kCGErrorIllegalArgument if there's no such mode.
int ah_vd_set_mode(uint32_t display, uint32_t width, uint32_t height, uint32_t pixel_width, uint32_t pixel_height) {
    @autoreleasepool {
        NSDictionary *options = @{(__bridge NSString *)kCGDisplayShowDuplicateLowResolutionModes : @YES};
        CFArrayRef modes = CGDisplayCopyAllDisplayModes(display, (__bridge CFDictionaryRef)options);
        if (!modes) return kCGErrorIllegalArgument;
        CGDisplayModeRef wanted = NULL;
        for (CFIndex i = 0; i < CFArrayGetCount(modes) && !wanted; i++) {
            CGDisplayModeRef mode = (CGDisplayModeRef)CFArrayGetValueAtIndex(modes, i);
            if (CGDisplayModeGetWidth(mode) == width && CGDisplayModeGetHeight(mode) == height &&
                CGDisplayModeGetPixelWidth(mode) == pixel_width && CGDisplayModeGetPixelHeight(mode) == pixel_height &&
                CGDisplayModeIsUsableForDesktopGUI(mode))
                wanted = mode;
        }
        CGError error = kCGErrorIllegalArgument;
        if (wanted) {
            CGDisplayConfigRef config;
            error = CGBeginDisplayConfiguration(&config);
            if (!error) {
                error = CGConfigureDisplayWithDisplayMode(config, display, wanted, NULL);
                if (error)
                    CGCancelDisplayConfiguration(config);
                else
                    error = CGCompleteDisplayConfiguration(config, kCGConfigurePermanently);
            }
        }
        CFRelease(modes);
        return error;
    }
}

// Put `display` into the mirror set of `of` (0: take it out of any mirror set), for this login session only.
// Returns a CGError.
int ah_vd_mirror(uint32_t display, uint32_t of) {
    CGDisplayConfigRef config;
    CGError error = CGBeginDisplayConfiguration(&config);
    if (error) return error;
    error = CGConfigureDisplayMirrorOfDisplay(config, display, of);
    if (error) {
        CGCancelDisplayConfiguration(config);
        return error;
    }
    return CGCompleteDisplayConfiguration(config, kCGConfigureForSession);
}
