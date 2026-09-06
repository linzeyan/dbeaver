// The first thing a Windows front end has to be able to do, asked on its own.
//
// Every other consumer of this core so far is Swift on macOS, and Swift reaches
// it through a modulemap and a SwiftPM link line — neither of which exists on
// Windows. So this asks the part underneath both: can a plain MSVC C++ program
// include `dbffi.h`, link `dbffi.lib`, and get an answer back. It deliberately
// pulls in no UI framework, so a failure here is about the core and the link
// line and nothing else. WinUI's own problems are worth meeting separately.
//
// `db_drivers_json` is the call because it needs no server, no handle, and no
// file: it reports what this build was compiled with. That makes a green run
// mean the whole staticlib was linked and its Rust runtime came up, not that
// one leaf function happened to be reachable.

#include <cstdio>
#include <cstring>

#include "dbffi.h"

int main() {
    char* err = nullptr;
    char* drivers = db_drivers_json(&err);

    if (drivers == nullptr) {
        // The core sets `err` on every failure path, but a null here with no
        // message would otherwise print as an empty line and read like success.
        std::printf("db_drivers_json failed: %s\n", err ? err : "(no message)");
        if (err != nullptr) {
            db_string_free(err);
        }
        return 1;
    }

    // Not a JSON parse — this file exists to test the link, and pulling in a
    // parser to check it would put a second thing in the failure path. The
    // drivers themselves are tested from Rust, where the whole catalog is.
    const bool looks_like_the_catalog =
        drivers[0] == '[' && std::strstr(drivers, "postgres") != nullptr;

    std::printf("db_drivers_json: %.200s\n", drivers);
    db_string_free(drivers);

    if (!looks_like_the_catalog) {
        std::printf("that is not the driver catalog\n");
        return 1;
    }
    return 0;
}
