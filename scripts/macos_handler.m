// SPDX-FileCopyrightText: 2025 The superseedr Contributors
// SPDX-License-Identifier: GPL-3.0-or-later

#import <AppKit/AppKit.h>
#import <CoreServices/CoreServices.h>
#import <UniformTypeIdentifiers/UniformTypeIdentifiers.h>
#include <fcntl.h>
#include <stdarg.h>
#include <string.h>
#include <unistd.h>

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
static NSString *const HandlerLogRelativeDirectory =
    @"com.github.jagalite.superseedr/logs/handler";
static NSString *const HandlerLogFilename = @"handler.log";
static const unsigned long long HandlerLogRotationBytes = 1024 * 1024;

static NSString *handlerLogDirectory(void) {
    NSArray<NSString *> *applicationSupportDirectories =
        NSSearchPathForDirectoriesInDomains(NSApplicationSupportDirectory,
                                            NSUserDomainMask, YES);
    if (applicationSupportDirectories.count == 0) {
        return nil;
    }
    return [applicationSupportDirectories.firstObject
        stringByAppendingPathComponent:HandlerLogRelativeDirectory];
}

static void rotateHandlerLogIfNeeded(NSString *logPath) {
    NSDictionary<NSFileAttributeKey, id> *attributes =
        [NSFileManager.defaultManager attributesOfItemAtPath:logPath error:nil];
    NSNumber *fileSize = attributes[NSFileSize];
    if (fileSize == nil || fileSize.unsignedLongLongValue < HandlerLogRotationBytes) {
        return;
    }

    NSString *rotatedPath = [logPath stringByAppendingString:@".1"];
    [NSFileManager.defaultManager removeItemAtPath:rotatedPath error:nil];
    [NSFileManager.defaultManager moveItemAtPath:logPath
                                          toPath:rotatedPath
                                           error:nil];
}

static void handlerLog(NSString *format, ...) NS_FORMAT_FUNCTION(1, 2);
static void handlerLog(NSString *format, ...) {
    va_list arguments;
    va_start(arguments, format);
    NSString *message = [[NSString alloc] initWithFormat:format arguments:arguments];
    va_end(arguments);

    // Keep a copy in Unified Logging as well as the user-accessible support log.
    NSLog(@"%@", message);

    NSString *logDirectory = handlerLogDirectory();
    if (logDirectory == nil) {
        return;
    }

    NSError *directoryError = nil;
    if (![NSFileManager.defaultManager
            createDirectoryAtPath:logDirectory
      withIntermediateDirectories:YES
                       attributes:@{NSFilePosixPermissions : @0700}
                            error:&directoryError]) {
        NSLog(@"Unable to create handler log directory: %@",
              directoryError.localizedDescription);
        return;
    }

    NSString *logPath = [logDirectory stringByAppendingPathComponent:HandlerLogFilename];
    rotateHandlerLogIfNeeded(logPath);

    NSString *timestamp = [NSISO8601DateFormatter stringFromDate:NSDate.date
                                                        timeZone:NSTimeZone.localTimeZone
                                                   formatOptions:NSISO8601DateFormatWithInternetDateTime];
    NSData *line = [[NSString stringWithFormat:@"%@ %@\n", timestamp, message]
        dataUsingEncoding:NSUTF8StringEncoding];
    int descriptor = open(logPath.fileSystemRepresentation,
                          O_WRONLY | O_CREAT | O_APPEND, 0600);
    if (descriptor < 0) {
        return;
    }

    const uint8_t *bytes = line.bytes;
    NSUInteger remaining = line.length;
    while (remaining > 0) {
        ssize_t written = write(descriptor, bytes, remaining);
        if (written <= 0) {
            break;
        }
        bytes += written;
        remaining -= (NSUInteger)written;
    }
    close(descriptor);
}

static NSString *defaultMagnetHandler(void) {
    return CFBridgingRelease(
        LSCopyDefaultHandlerForURLScheme((__bridge CFStringRef)MagnetScheme));
}

static NSString *defaultTorrentHandler(void) {
    return CFBridgingRelease(LSCopyDefaultRoleHandlerForContentType(
        (__bridge CFStringRef)TorrentTypeIdentifier, kLSRolesAll));
}

static BOOL handlerMatches(NSString *handler) {
    return [handler isEqualToString:SuperseedrBundleIdentifier];
}

static BOOL magnetHandlerIsCurrent(void) {
    return handlerMatches(defaultMagnetHandler());
}

static BOOL torrentHandlerIsCurrent(void) {
    return handlerMatches(defaultTorrentHandler());
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
- (void)finishRegistrationUpdate:(NSError *)error label:(NSString *)label;
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
    handlerLog(@"handler launched mode=%@",
               self.forceRegistration ? @"registration" : @"open");
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
    if (self.forceRegistration) {
        // The package installer launches the app solely to change defaults.
        // Ignore any synthetic open event LaunchServices sends at startup.
        return;
    }

    BOOL handledOpenEvent = NO;
    for (NSURL *url in urls) {
        if (url.isFileURL) {
            [self forwardSource:url.path];
            handledOpenEvent = YES;
        } else if ([url.scheme caseInsensitiveCompare:MagnetScheme] == NSOrderedSame) {
            [self forwardSource:url.absoluteString];
            handledOpenEvent = YES;
        } else {
            handlerLog(@"ignored unsupported URL scheme=%@", url.scheme);
        }
    }

    // LaunchServices can deliver an empty openURLs callback during an ordinary
    // application launch. Do not let that callback cancel the registration
    // path used by the package postinstall script.
    if (handledOpenEvent) {
        self.receivedOpenEvent = YES;
        [self scheduleTermination];
    }
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
        handlerLog(@"forward failed kind=%@ error_domain=%@ error_code=%ld",
                   [source hasPrefix:@"magnet:"] ? @"magnet" : @"torrent",
                   error.domain, (long)error.code);
        return;
    }
    handlerLog(@"forward launched kind=%@ pid=%d",
               [source hasPrefix:@"magnet:"] ? @"magnet" : @"torrent",
               task.processIdentifier);
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
    NSString *magnetBefore = defaultMagnetHandler() ?: @"<none>";
    NSString *torrentBefore = defaultTorrentHandler() ?: @"<none>";
    BOOL updateMagnetHandler = !magnetHandlerIsCurrent();
    BOOL updateTorrentHandler = !torrentHandlerIsCurrent();
    handlerLog(@"registration starting magnet_before=%@ torrent_before=%@",
               magnetBefore, torrentBefore);
    if (!updateMagnetHandler && !updateTorrentHandler) {
        handlerLog(@"registration skipped reason=already_current");
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
        handlerLog(@"registration soft_failed api=legacy magnet_status=%d torrent_status=%d",
                   magnetStatus, torrentStatus);
    } else {
        handlerLog(@"registration verified api=legacy magnet=yes torrent=yes");
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
                                   [self finishRegistrationUpdate:error label:@"magnet"];
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
                                   [self finishRegistrationUpdate:error label:@"torrent"];
                               });
                           }];
    }

    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 60 * NSEC_PER_SEC),
                   dispatch_get_main_queue(), ^{
                       if (self.pendingRegistrationUpdates > 0) {
                           handlerLog(@"registration soft_failed reason=timeout pending=%ld",
                                      (long)self.pendingRegistrationUpdates);
                           [NSApp terminate:nil];
                       }
                   });
}

- (void)finishRegistrationUpdate:(NSError *)error
                            label:(NSString *)label API_AVAILABLE(macos(12.0)) {
    if (error != nil) {
        self.registrationFailed = YES;
        handlerLog(@"registration update soft_failed kind=%@ error_domain=%@ error_code=%ld",
                   label, error.domain, (long)error.code);
    } else {
        handlerLog(@"registration update completed kind=%@", label);
    }
    self.pendingRegistrationUpdates -= 1;
    if (self.pendingRegistrationUpdates == 0) {
        dispatch_after(dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC),
                       dispatch_get_main_queue(), ^{
                           if (!self.registrationFailed && !handlersAreCurrent()) {
                               handlerLog(@"registration soft_failed reason=verification");
                           } else if (!self.registrationFailed) {
                               handlerLog(@"registration verified api=modern magnet=yes torrent=yes");
                           }
                           [NSApp terminate:nil];
                       });
    }
}

@end

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        if (argc == 2 && strcmp(argv[1], "--verify") == 0) {
            NSString *magnet = defaultMagnetHandler() ?: @"<none>";
            NSString *torrent = defaultTorrentHandler() ?: @"<none>";
            BOOL verified = handlersAreCurrent();
            printf("magnet=%s\ntorrent=%s\nverified=%s\n",
                   magnet.UTF8String, torrent.UTF8String, verified ? "true" : "false");
            handlerLog(@"verification magnet=%@ torrent=%@ verified=%@",
                       magnet, torrent, verified ? @"yes" : @"no");
            return verified ? EXIT_SUCCESS : EXIT_FAILURE;
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
