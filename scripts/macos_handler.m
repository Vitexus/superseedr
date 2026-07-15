// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#import <AppKit/AppKit.h>
#import <CoreServices/CoreServices.h>
#import <UniformTypeIdentifiers/UniformTypeIdentifiers.h>
#include <string.h>

#ifndef SUPERSEEDR_BINARY_PATH
#define SUPERSEEDR_BINARY_PATH "/usr/local/bin/superseedr"
#endif
#ifndef SUPERSEEDR_BUNDLE_IDENTIFIER
#define SUPERSEEDR_BUNDLE_IDENTIFIER "com.github.jagalite.superseedr"
#endif
#ifndef SUPERSEEDR_MAGNET_SCHEME
#define SUPERSEEDR_MAGNET_SCHEME "magnet"
#endif
#ifndef SUPERSEEDR_TORRENT_TYPE_IDENTIFIER
#define SUPERSEEDR_TORRENT_TYPE_IDENTIFIER "org.bittorrent.torrent"
#endif

static NSString *const SuperseedrBundleIdentifier = @SUPERSEEDR_BUNDLE_IDENTIFIER;
static NSString *const SuperseedrBinaryPath = @SUPERSEEDR_BINARY_PATH;
static NSString *const MagnetScheme = @SUPERSEEDR_MAGNET_SCHEME;
static NSString *const TorrentTypeIdentifier = @SUPERSEEDR_TORRENT_TYPE_IDENTIFIER;

static BOOL handlerMatches(CFStringRef handler) {
    if (handler == NULL) {
        return NO;
    }

    BOOL matches = CFEqual(handler, (__bridge CFStringRef)SuperseedrBundleIdentifier);
    CFRelease(handler);
    return matches;
}

static BOOL magnetHandlerIsCurrent(void) {
    return handlerMatches(
        LSCopyDefaultHandlerForURLScheme((__bridge CFStringRef)MagnetScheme));
}

static BOOL torrentHandlerIsCurrent(void) {
    return handlerMatches(LSCopyDefaultRoleHandlerForContentType(
        (__bridge CFStringRef)TorrentTypeIdentifier, kLSRolesAll));
}

static BOOL handlersAreCurrent(void) {
    return magnetHandlerIsCurrent() && torrentHandlerIsCurrent();
}

@interface SuperseedrHandlerDelegate : NSObject <NSApplicationDelegate>
@property(nonatomic) BOOL forceRegistration;
@property(nonatomic) BOOL receivedOpenEvent;
@property(nonatomic) BOOL terminationScheduled;
@property(nonatomic) NSInteger pendingRegistrationUpdates;
@property(nonatomic) BOOL registrationFailed;
@end

@implementation SuperseedrHandlerDelegate

- (instancetype)initWithForceRegistration:(BOOL)forceRegistration {
    self = [super init];
    if (self != nil) {
        _forceRegistration = forceRegistration;
    }
    return self;
}

- (void)applicationDidFinishLaunching:(NSNotification *)notification {
    (void)notification;
    NSTimeInterval delay = self.forceRegistration ? 0.0 : 0.5;
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, (int64_t)(delay * NSEC_PER_SEC)),
                   dispatch_get_main_queue(), ^{
                       if (!self.receivedOpenEvent) {
                           [self registerDefaultHandlers];
                       }
                   });
}

- (void)application:(NSApplication *)application openURLs:(NSArray<NSURL *> *)urls {
    (void)application;
    self.receivedOpenEvent = YES;
    for (NSURL *url in urls) {
        if (url.isFileURL) {
            [self forwardSource:url.path];
        } else if ([url.scheme caseInsensitiveCompare:MagnetScheme] == NSOrderedSame) {
            [self forwardSource:url.absoluteString];
        } else {
            NSLog(@"Ignoring unsupported URL scheme: %@", url.scheme);
        }
    }
    [self scheduleTermination];
}

- (void)forwardSource:(NSString *)source {
    if (source.length == 0) {
        return;
    }

    NSTask *task = [[NSTask alloc] init];
    task.executableURL = [NSURL fileURLWithPath:SuperseedrBinaryPath];
    task.arguments = @[ source ];
    task.standardOutput = [NSFileHandle fileHandleWithNullDevice];
    task.standardError = [NSFileHandle fileHandleWithNullDevice];

    NSError *error = nil;
    if (![task launchAndReturnError:&error]) {
        NSLog(@"Unable to forward source to superseedr: %@", error.localizedDescription);
    }
}

- (void)scheduleTermination {
    if (self.terminationScheduled) {
        return;
    }
    self.terminationScheduled = YES;
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 200 * NSEC_PER_MSEC),
                   dispatch_get_main_queue(), ^{
                       [NSApp terminate:nil];
                   });
}

- (void)registerDefaultHandlers {
    BOOL updateMagnetHandler = !magnetHandlerIsCurrent();
    BOOL updateTorrentHandler = !torrentHandlerIsCurrent();
    if (!updateMagnetHandler && !updateTorrentHandler) {
        [NSApp terminate:nil];
        return;
    }

    if (@available(macOS 12.0, *)) {
        [self registerModernDefaultHandlersForMagnet:updateMagnetHandler
                                             torrent:updateTorrentHandler];
        return;
    }

    OSStatus magnetStatus = noErr;
    if (updateMagnetHandler) {
        magnetStatus = LSSetDefaultHandlerForURLScheme(
            (__bridge CFStringRef)MagnetScheme,
            (__bridge CFStringRef)SuperseedrBundleIdentifier);
    }

    OSStatus torrentStatus = noErr;
    if (updateTorrentHandler) {
        torrentStatus = LSSetDefaultRoleHandlerForContentType(
            (__bridge CFStringRef)TorrentTypeIdentifier,
            kLSRolesAll,
            (__bridge CFStringRef)SuperseedrBundleIdentifier);
    }

    if (magnetStatus != noErr || torrentStatus != noErr || !handlersAreCurrent()) {
        NSLog(@"Unable to set default handlers (%d, %d)", magnetStatus, torrentStatus);
    }
    [NSApp terminate:nil];
}

- (void)registerModernDefaultHandlersForMagnet:(BOOL)updateMagnetHandler
                                       torrent:(BOOL)updateTorrentHandler API_AVAILABLE(macos(12.0)) {
    NSURL *applicationURL = NSBundle.mainBundle.bundleURL;
    NSWorkspace *workspace = NSWorkspace.sharedWorkspace;
    self.pendingRegistrationUpdates = updateMagnetHandler + updateTorrentHandler;
    self.registrationFailed = NO;

    if (updateMagnetHandler) {
        [workspace setDefaultApplicationAtURL:applicationURL
                        toOpenURLsWithScheme:MagnetScheme
                           completionHandler:^(NSError *error) {
                               dispatch_async(dispatch_get_main_queue(), ^{
                                   [self finishRegistrationUpdate:error];
                               });
                           }];
    }

    if (updateTorrentHandler) {
        UTType *torrentType = [UTType importedTypeWithIdentifier:TorrentTypeIdentifier
                                               conformingToType:UTTypeData];
        [workspace setDefaultApplicationAtURL:applicationURL
                            toOpenContentType:torrentType
                           completionHandler:^(NSError *error) {
                               dispatch_async(dispatch_get_main_queue(), ^{
                                   [self finishRegistrationUpdate:error];
                               });
                           }];
    }

    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 60 * NSEC_PER_SEC),
                   dispatch_get_main_queue(), ^{
                       if (self.pendingRegistrationUpdates > 0) {
                           NSLog(@"Timed out while setting default handlers");
                           [NSApp terminate:nil];
                       }
                   });
}

- (void)finishRegistrationUpdate:(NSError *)error API_AVAILABLE(macos(12.0)) {
    if (error != nil) {
        self.registrationFailed = YES;
        NSLog(@"Unable to set a default handler: %@", error.localizedDescription);
    }
    self.pendingRegistrationUpdates -= 1;
    if (self.pendingRegistrationUpdates == 0) {
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC),
                       dispatch_get_main_queue(), ^{
                           if (!self.registrationFailed && !handlersAreCurrent()) {
                               NSLog(@"Default handler verification failed");
                           }
                           [NSApp terminate:nil];
                       });
    }
}

@end

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc == 2 && strcmp(argv[1], "--verify") == 0) {
            return handlersAreCurrent() ? EXIT_SUCCESS : EXIT_FAILURE;
        }

        BOOL forceRegistration =
            argc == 2 && strcmp(argv[1], "--register-handlers") == 0;
        NSApplication *application = NSApplication.sharedApplication;
        SuperseedrHandlerDelegate *delegate =
            [[SuperseedrHandlerDelegate alloc] initWithForceRegistration:forceRegistration];
        application.delegate = delegate;
        [application run];
    }
    return EXIT_SUCCESS;
}
