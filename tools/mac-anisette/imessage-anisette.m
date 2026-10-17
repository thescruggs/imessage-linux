// imessage-anisette: print this Mac's native anisette headers as JSON.
//
// Since 2026-10-13 Apple's GrandSlam (gsa.apple.com) rejects logins whose
// anisette headers come from the Android-based anisette-v3-server software
// that every public anisette server runs (error -80045, "This action cannot
// be completed at this time"), while headers minted by a real Mac's
// AOSKit/AuthKit still work. imessaged therefore runs this tool on your Mac
// (over ssh, `anisette_command` in config.toml) whenever it needs to
// authenticate with Apple: at login, and at the rare silent re-login after
// Apple drops the IDS auth. Message traffic never touches the Mac.
//
// Build (Command Line Tools):
//   clang -fobjc-arc -framework Foundation -o imessage-anisette imessage-anisette.m
// or let tools/mac-anisette-install.sh do it over ssh.
//
// Output: one JSON object with X-Apple-I-MD (one-time password),
// X-Apple-I-MD-M (machine id), X-Apple-I-MD-LU, X-Mme-Device-Id,
// X-Apple-I-MD-RINFO, X-Mme-Client-Info and X-Apple-Locale. Everything
// time-dependent (X-Apple-I-Client-Time, X-Apple-I-TimeZone) is added by
// the daemon when it uses the headers. The same AOSKit/AuthKit calls that
// omnisette's `aos_kit.rs` makes when running natively on a Mac.

#import <Foundation/Foundation.h>
#import <dlfcn.h>

static NSString *str(id obj) {
    return [obj isKindOfClass:[NSString class]] ? obj : (obj ? [obj description] : nil);
}

int main(int argc, const char **argv) {
    @autoreleasepool {
        if (!dlopen("/System/Library/PrivateFrameworks/AOSKit.framework/AOSKit", RTLD_NOW)) {
            fprintf(stderr, "imessage-anisette: cannot load AOSKit: %s\n", dlerror());
            return 2;
        }
        if (!dlopen("/System/Library/PrivateFrameworks/AuthKit.framework/AuthKit", RTLD_NOW)) {
            fprintf(stderr, "imessage-anisette: cannot load AuthKit: %s\n", dlerror());
            return 2;
        }
        Class AOSUtilities = NSClassFromString(@"AOSUtilities");
        Class AKDevice = NSClassFromString(@"AKDevice");
        if (!AOSUtilities || !AKDevice) {
            fprintf(stderr, "imessage-anisette: AOSUtilities/AKDevice class missing\n");
            return 2;
        }

        // AOSKit: the attested pair (OTP + machine id). "-2" = no DSID.
        NSDictionary *otp = [AOSUtilities performSelector:@selector(retrieveOTPHeadersForDSID:) withObject:@"-2"];
        NSString *md = str(otp[@"X-Apple-MD"]);
        NSString *mdm = str(otp[@"X-Apple-MD-M"]);
        if (md.length == 0 || mdm.length == 0) {
            // Neutral names only: the Linux side redacts any stderr line
            // naming an anisette header, which would hide this diagnostic.
            if (!otp) {
                fprintf(stderr, "imessage-anisette: AOSKit returned no otp dictionary\n");
            } else {
                const char *missing = md.length == 0 && mdm.length == 0 ? "one-time password, machine id"
                                    : md.length == 0                    ? "one-time password"
                                                                        : "machine id";
                fprintf(stderr, "imessage-anisette: otp dictionary has %d of 2 required keys (missing: %s; %lu keys in total)\n",
                        (md.length != 0) + (mdm.length != 0), missing, (unsigned long)otp.count);
            }
            return 3;
        }

        // AuthKit: the device description the headers belong to.
        id device = [AKDevice performSelector:@selector(currentDevice)];
        NSString *lu = str([device performSelector:@selector(localUserUUID)]);
        NSString *deviceId = str([device performSelector:@selector(uniqueDeviceIdentifier)]);
        NSString *clientInfo = str([device performSelector:@selector(serverFriendlyDescription)]);
        NSString *locale = str([[device performSelector:@selector(locale)] performSelector:@selector(localeIdentifier)]);
        if (deviceId.length == 0) deviceId = str([AOSUtilities performSelector:@selector(machineUDID)]);

        NSMutableDictionary *out = [NSMutableDictionary dictionary];
        out[@"X-Apple-I-MD"] = md;
        out[@"X-Apple-I-MD-M"] = mdm;
        out[@"X-Apple-I-MD-RINFO"] = @"17106176";
        if (lu.length) out[@"X-Apple-I-MD-LU"] = lu;
        if (deviceId.length) out[@"X-Mme-Device-Id"] = deviceId;
        if (clientInfo.length) out[@"X-Mme-Client-Info"] = clientInfo;
        if (locale.length) out[@"X-Apple-Locale"] = locale;

        NSError *err = nil;
        NSData *json = [NSJSONSerialization dataWithJSONObject:out options:NSJSONWritingSortedKeys error:&err];
        if (!json) {
            fprintf(stderr, "imessage-anisette: JSON error: %s\n", [[err description] UTF8String]);
            return 2;
        }
        fwrite(json.bytes, 1, json.length, stdout);
        fputc('\n', stdout);
    }
    return 0;
}
