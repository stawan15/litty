//! Puts the finished framebuffer on screen.
//!
//! macOS: the pixels go into IOSurfaces that Core Animation shows directly. (softbuffer hands
//! Core Animation a CGImage, which it copies in full on every frame: ~14 ms and 25 MB.)
//! Elsewhere: softbuffer.

use std::sync::Arc;
use winit::window::Window;

#[cfg(target_os = "macos")]
pub use mac::Presenter;
#[cfg(not(target_os = "macos"))]
pub use other::Presenter;

#[cfg(not(target_os = "macos"))]
mod other {
    use super::*;
    use std::num::NonZeroU32;

    pub struct Presenter(softbuffer::Surface<Arc<Window>, Arc<Window>>);

    impl Presenter {
        pub fn new(window: &Arc<Window>) -> Self {
            let ctx = softbuffer::Context::new(window.clone()).unwrap();
            Presenter(softbuffer::Surface::new(&ctx, window.clone()).unwrap())
        }

        pub fn resize(&mut self, w: usize, h: usize) {
            if let (Some(w), Some(h)) = (NonZeroU32::new(w as u32), NonZeroU32::new(h as u32)) {
                self.0.resize(w, h).unwrap();
            }
        }

        pub fn present(&mut self, fb: &[u32], damage: Option<(usize, usize)>) {
            if damage.is_none() {
                return;
            }
            let mut buf = self.0.buffer_mut().unwrap();
            if buf.len() == fb.len() {
                buf.copy_from_slice(fb);
                buf.present().unwrap();
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Bool};
    use objc2_core_foundation::{CFDictionary, CFRetained, CGPoint, CGRect, CGSize};
    use objc2_foundation::{NSDictionary, NSNumber, NSString, ns_string};
    use objc2_io_surface::{IOSurfaceLockOptions, IOSurfaceRef};
    use objc2_quartz_core::{CALayer, CATransaction, kCAGravityTopLeft};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    /// `kCVPixelFormatType_32BGRA`: little-endian 0xAARRGGBB words, which is the framebuffer layout.
    const BGRA: u32 = 0x4247_5241;

    pub struct Presenter {
        window: Arc<Window>,
        layer: Retained<CALayer>,
        /// Surfaces of the current size; the compositor may still be reading the last shown one,
        /// so each frame goes into another one.
        surfaces: Vec<CFRetained<IOSurfaceRef>>,
        /// Per surface: rows that changed since it was last written.
        stale: Vec<Option<(usize, usize)>>,
        shown: usize,
        size: (usize, usize),
    }

    impl Presenter {
        pub fn new(window: &Arc<Window>) -> Self {
            let RawWindowHandle::AppKit(handle) = window.window_handle().unwrap().as_raw() else { panic!("not an AppKit window") };
            // SAFETY: the handle holds a valid NSView and winit creates windows on the main thread.
            let view: &AnyObject = unsafe { handle.ns_view.cast().as_ref() };
            let _: () = unsafe { msg_send![view, setWantsLayer: Bool::YES] };
            let root: Retained<CALayer> = unsafe { msg_send![view, layer] };
            let layer = CALayer::new();
            layer.setAnchorPoint(CGPoint::new(0.0, 0.0));
            layer.setGeometryFlipped(true);
            layer.setOpaque(true);
            layer.setContentsGravity(unsafe { kCAGravityTopLeft });
            root.addSublayer(&layer);
            Presenter { window: window.clone(), layer, surfaces: Vec::new(), stale: Vec::new(), shown: 0, size: (0, 0) }
        }

        pub fn resize(&mut self, w: usize, h: usize) {
            self.size = (w, h);
            self.surfaces.clear();
            self.stale.clear();
            let scale = self.window.scale_factor();
            self.layer.setContentsScale(scale);
            self.layer.setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(w as f64 / scale, h as f64 / scale)));
        }

        fn new_surface(&self) -> Option<CFRetained<IOSurfaceRef>> {
            let num = |n: usize| NSNumber::new_usize(n);
            let keys = [ns_string!("IOSurfaceWidth"), ns_string!("IOSurfaceHeight"), ns_string!("IOSurfaceBytesPerElement"), ns_string!("IOSurfacePixelFormat")];
            let vals = [num(self.size.0), num(self.size.1), num(4), NSNumber::new_u32(BGRA)];
            let dict: Retained<NSDictionary<NSString, NSNumber>> = NSDictionary::from_retained_objects(&keys, &vals);
            // SAFETY: NSDictionary is toll-free bridged to CFDictionary; the keys/values are the types IOSurface expects.
            unsafe { IOSurfaceRef::new(&*(Retained::as_ptr(&dict) as *const CFDictionary)) }
        }

        /// Show `fb`, of which only rows `damage` (first, last + 1) changed since the last call.
        pub fn present(&mut self, fb: &[u32], damage: Option<(usize, usize)>) {
            let Some((d0, d1)) = damage else { return };
            if self.size.0 == 0 || fb.len() != self.size.0 * self.size.1 {
                return;
            }
            for st in &mut self.stale {
                *st = Some(st.map_or((d0, d1), |(a, b)| (a.min(d0), b.max(d1))));
            }
            // Round-robin from the last shown one; a third surface is only allocated when the
            // compositor still holds the others.
            let n = self.surfaces.len();
            let free = (1..=n).map(|k| (self.shown + k) % n).find(|&i| i != self.shown && !self.surfaces[i].is_in_use());
            let i = match free {
                Some(i) => i,
                None if n < 3 => {
                    let Some(s) = self.new_surface() else { return };
                    self.surfaces.push(s);
                    self.stale.push(Some((0, self.size.1)));
                    n
                }
                None => (self.shown + 1) % n,
            };
            let s = &self.surfaces[i];
            let (w, stride) = (self.size.0, s.bytes_per_row() / 4);
            let (r0, r1) = self.stale[i].take().unwrap_or((0, 0));
            // SAFETY: the surface is locked while we write `w` words in each of rows r0..r1 (< height).
            // The framebuffer's alpha byte is 0, so it is set here (else the layer blends with what is behind it).
            unsafe {
                s.lock(IOSurfaceLockOptions::empty(), std::ptr::null_mut());
                let dst = s.base_address().as_ptr() as *mut u32;
                for y in r0..r1 {
                    let out = std::slice::from_raw_parts_mut(dst.add(y * stride), w);
                    for (o, &px) in out.iter_mut().zip(&fb[y * w..(y + 1) * w]) {
                        *o = px | 0xFF00_0000;
                    }
                }
                s.unlock(IOSurfaceLockOptions::empty(), std::ptr::null_mut());
            }
            // Without this, changing `contents` fades between the old and new frame.
            CATransaction::begin();
            CATransaction::setDisableActions(true);
            // SAFETY: an IOSurface is a valid `contents` value.
            unsafe { self.layer.setContents(Some(&*(&**s as *const IOSurfaceRef as *const AnyObject))) };
            CATransaction::commit();
            self.shown = i;
        }
    }
}
