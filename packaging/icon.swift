// Draws the app icon: a dark rounded square with a prompt chevron and a cursor block.
import AppKit
let size = 1024
let img = NSImage(size: NSSize(width: size, height: size))
img.lockFocus()
let s = CGFloat(size)
let bg = NSBezierPath(roundedRect: NSRect(x: 100, y: 100, width: s - 200, height: s - 200), xRadius: 190, yRadius: 190)
NSColor(red: 0x1a / 255, green: 0x1b / 255, blue: 0x26 / 255, alpha: 1).setFill()
bg.fill()
NSColor(red: 0x7a / 255, green: 0xa2 / 255, blue: 0xf7 / 255, alpha: 1).setStroke()
let chev = NSBezierPath()
chev.lineWidth = 64
chev.lineCapStyle = .round
chev.lineJoinStyle = .round
chev.move(to: NSPoint(x: 330, y: 660))
chev.line(to: NSPoint(x: 500, y: 512))
chev.line(to: NSPoint(x: 330, y: 364))
chev.stroke()
NSColor(red: 0xc0 / 255, green: 0xca / 255, blue: 0xf5 / 255, alpha: 1).setFill()
NSBezierPath(roundedRect: NSRect(x: 580, y: 350, width: 130, height: 240), xRadius: 14, yRadius: 14).fill()
img.unlockFocus()
let rep = NSBitmapImageRep(data: img.tiffRepresentation!)!
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
