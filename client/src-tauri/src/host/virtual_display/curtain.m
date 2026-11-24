// Privacy mode on macOS (see curtain.rs): black windows over the local displays, on top of everything (only the
// pointer can be above them). They ignore the mouse, never become key and stay on every Space. Airhand's own screen
// capture leaves them out (SCContentFilter excludingWindows, see host/sck.rs), so the controller sees what is
// under them. They back up the black gamma tables: on some Macs (M5 Pro / Max, MacBook Neo on macOS 26) the
// system accepts a gamma table and then doesn't show it, and AirPlay / Sidecar / DisplayLink displays have none.
//
// AppKit windows belong to the main thread: callers on other threads ask for a set of displays (ah_cover) and get a
// ticket; the main queue carries the requests out in order (ah_cover_done says how far it got). The window numbers
// of the covers on screen are kept under a lock for the capture threads (ah_cover_windows), and every change to them
// bumps a generation (ah_cover_generation): captures opened before it reopen without the new covers.
#import <AppKit/AppKit.h>
#import <os/lock.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>

#define MAX_COVERS 32

// Main thread only: display ID -> its cover
static NSMutableDictionary<NSNumber *, NSWindow *> *covers;

static os_unfair_lock shownLock = OS_UNFAIR_LOCK_INIT;
static uint32_t shown[MAX_COVERS];  // window numbers, sorted (under shownLock)
static size_t shownCount;

static _Atomic uint64_t requested;
static _Atomic uint64_t done;
static _Atomic uint64_t generation;

static NSScreen *screenOf(CGDirectDisplayID display) {
    for (NSScreen *s in NSScreen.screens) {
        NSNumber *n = s.deviceDescription[@"NSScreenNumber"];
        if ([n isKindOfClass:NSNumber.class] && n.unsignedIntValue == display) return s;
    }
    return nil;
}

static NSWindow *makeCover(NSRect frame) {
    NSWindow *w = [[NSWindow alloc] initWithContentRect:frame
                                              styleMask:NSWindowStyleMaskBorderless
                                                backing:NSBackingStoreBuffered
                                                  defer:NO];
    w.releasedWhenClosed = NO;
    // Not quite opaque: a fully opaque window makes every window under it "occluded" (NSWindowOcclusionState), and
    // apps stop drawing occluded windows (browsers throttle them), so the controller would see a frozen picture.
    // At alpha 0.999 the windows under it stay visible and the screen still reads as rgb(0, 0, 0) (measured on
    // macOS 27: a red window under it, captured with and without the cover).
    w.opaque = NO;
    w.backgroundColor = [NSColor colorWithWhite:0 alpha:0.999];
    w.hasShadow = NO;
    w.level = CGWindowLevelForKey(kCGMaximumWindowLevelKey);
    // Clicks (the controller's too) go to whatever is under it
    w.ignoresMouseEvents = YES;
    // Hiding Airhand (Command-H, "Hide Others") doesn't take it away
    w.canHide = NO;
    w.excludedFromWindowsMenu = YES;
    w.animationBehavior = NSWindowAnimationBehaviorNone;
    w.tabbingMode = NSWindowTabbingModeDisallowed;
    w.collectionBehavior = NSWindowCollectionBehaviorCanJoinAllSpaces | NSWindowCollectionBehaviorStationary |
                           NSWindowCollectionBehaviorIgnoresCycle | NSWindowCollectionBehaviorFullScreenAuxiliary;
    return w;
}

static int compareNumbers(const void *a, const void *b) {
    uint32_t x = *(const uint32_t *)a, y = *(const uint32_t *)b;
    return (x > y) - (x < y);
}

// Main thread: cover exactly the screens among `wanted` (displays that aren't screens, e.g. the mirrors in a
// mirror set, get nothing), bring the covers to the front, publish their window numbers.
static void apply(NSArray<NSNumber *> *wanted, uint64_t ticket) {
    NSMutableDictionary<NSNumber *, NSWindow *> *keep = [NSMutableDictionary dictionary];
    for (NSNumber *display in wanted) {
        if (keep.count >= MAX_COVERS) break;
        NSScreen *screen = screenOf(display.unsignedIntValue);
        if (!screen) continue;
        NSWindow *w = covers[display];
        if (!w) w = makeCover(screen.frame);
        if (!NSEqualRects(w.frame, screen.frame)) [w setFrame:screen.frame display:YES];
        // Another window may have gone up to the same level since
        [w orderFrontRegardless];
        keep[display] = w;
    }
    for (NSNumber *display in covers) {
        if (keep[display] == nil) {
            NSWindow *w = covers[display];
            [w orderOut:nil];
            [w close];
        }
    }
    covers = keep;

    uint32_t numbers[MAX_COVERS];
    size_t n = 0;
    for (NSWindow *w in keep.allValues) numbers[n++] = (uint32_t)w.windowNumber;
    qsort(numbers, n, sizeof numbers[0], compareNumbers);
    os_unfair_lock_lock(&shownLock);
    bool changed = n != shownCount || memcmp(numbers, shown, n * sizeof numbers[0]) != 0;
    memcpy(shown, numbers, n * sizeof numbers[0]);
    shownCount = n;
    os_unfair_lock_unlock(&shownLock);
    if (changed) atomic_fetch_add(&generation, 1);
    atomic_store(&done, ticket);
}

// Cover these displays (count 0: take every cover away). Returns the ticket of this request.
uint64_t ah_cover(const uint32_t *displays, size_t count) {
    NSMutableArray<NSNumber *> *wanted = [NSMutableArray arrayWithCapacity:count];
    for (size_t i = 0; i < count; i++) [wanted addObject:@(displays[i])];
    uint64_t ticket = atomic_fetch_add(&requested, 1) + 1;
    dispatch_async(dispatch_get_main_queue(), ^{
        @autoreleasepool {
            apply(wanted, ticket);
        }
    });
    return ticket;
}

// The latest request the main thread has carried out (requests are carried out in order).
uint64_t ah_cover_done(void) { return atomic_load(&done); }

// Bumped whenever the covers on screen change.
uint64_t ah_cover_generation(void) { return atomic_load(&generation); }

// Window numbers of the covers on screen; returns how many there are (at most `capacity` are written).
size_t ah_cover_windows(uint32_t *windows, size_t capacity) {
    os_unfair_lock_lock(&shownLock);
    size_t n = shownCount;
    memcpy(windows, shown, (n < capacity ? n : capacity) * sizeof shown[0]);
    os_unfair_lock_unlock(&shownLock);
    return n;
}
