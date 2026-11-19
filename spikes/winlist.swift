// Lists the windows of one process as the window server sees them.
//   winlist <pid>
import CoreGraphics
import Foundation

guard CommandLine.arguments.count > 1, let pid = Int32(CommandLine.arguments[1]) else {
    print("usage: winlist <pid>")
    exit(2)
}
let info = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as? [[String: Any]] ?? []
var found = 0
for w in info where (w[kCGWindowOwnerPID as String] as? Int32) == pid {
    found += 1
    let b = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
    print("window \(w[kCGWindowNumber as String] ?? "?") layer=\(w[kCGWindowLayer as String] ?? "?") onscreen=\(w[kCGWindowIsOnscreen as String] ?? false) alpha=\(w[kCGWindowAlpha as String] ?? "?") bounds=\(b["X"] ?? "?"),\(b["Y"] ?? "?") \(b["Width"] ?? "?")x\(b["Height"] ?? "?")")
}
if found == 0 { print("no windows for pid \(pid)") }
