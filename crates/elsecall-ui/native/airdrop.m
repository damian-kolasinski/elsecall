#import <AppKit/AppKit.h>

// This bridge owns transport presentation only. All calls run on rune's main
// thread. Keep the service and its weak delegate alive until Rust disposes them.
@interface ElsecallAirDrop : NSObject <NSSharingServiceDelegate>
@property(strong) NSSharingService *service;
@property(strong) NSWindow *source;
@property NSInteger result;
@end

@implementation ElsecallAirDrop
- (void)sharingService:(NSSharingService *)service didShareItems:(NSArray *)items {
    self.result = 1; // AirDrop can call this even after Cancel. Delivery is unconfirmed.
}
- (void)sharingService:(NSSharingService *)service didFailToShareItems:(NSArray *)items error:(NSError *)error {
    // Never surface NSError descriptions: they can include paths or recipients.
    self.result = ([error.domain isEqualToString:NSCocoaErrorDomain] && error.code == NSUserCancelledError) ? 2 : 3;
}
- (NSWindow *)sharingService:(NSSharingService *)service sourceWindowForShareItems:(NSArray *)items sharingContentScope:(NSSharingContentScope *)scope {
    *scope = NSSharingContentScopeItem;
    return self.source;
}
- (NSView *)anchoringViewForSharingService:(NSSharingService *)service showRelativeToRect:(NSRect *)rect preferredEdge:(NSRectEdge *)edge {
    NSView *view = self.source.contentView;
    *rect = NSMakeRect(NSMaxX(view.bounds) - 160, NSMaxY(view.bounds) - 64, 100, 32);
    *edge = NSMinYEdge;
    return view;
}
@end

void *elsecall_airdrop_start(const char *path) {
    NSCAssert(NSThread.isMainThread, @"AirDrop requires the main thread");
    NSWindow *window = NSApp.keyWindow;
    if (!window) return NULL;
    NSString *file = [[NSFileManager defaultManager] stringWithFileSystemRepresentation:path length:strlen(path)];
    NSArray *items = @[[NSURL fileURLWithPath:file]];
    NSSharingService *service = [NSSharingService sharingServiceNamed:NSSharingServiceNameSendViaAirDrop];
    if (!service || ![service canPerformWithItems:items]) return NULL;
    ElsecallAirDrop *transfer = [ElsecallAirDrop new];
    transfer.source = window;
    transfer.service = service;
    service.delegate = transfer;
    [service performWithItems:items];
    return (__bridge_retained void *)transfer;
}

int elsecall_airdrop_result(void *handle) {
    NSCAssert(NSThread.isMainThread, @"AirDrop requires the main thread");
    return (int)((__bridge ElsecallAirDrop *)handle).result;
}

void elsecall_airdrop_release(void *handle) {
    NSCAssert(NSThread.isMainThread, @"AirDrop requires the main thread");
    ElsecallAirDrop *transfer = (__bridge_transfer ElsecallAirDrop *)handle;
    transfer.service.delegate = nil;
}
