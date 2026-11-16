//! The title bar that fades: the frame fills the whole window, and
//! a strip of the title bar's height, in the system's material for one, fades in over its top
//! with the system's own bar while the pointer moves over the window.

use std::cell::Cell;
use std::ptr::NonNull;

use block2::StackBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, define_class};
use objc2::{msg_send, sel};
use objc2_app_kit::{
    NSAnimatablePropertyContainer, NSAnimationContext, NSAutoresizingMaskOptions, NSEvent,
    NSResponder, NSTrackingArea, NSTrackingAreaOptions, NSView, NSVisualEffectBlendingMode,
    NSVisualEffectMaterial, NSVisualEffectView, NSWindow, NSWindowButton, NSWindowOrderingMode,
    NSWindowStyleMask,
};
use objc2_foundation::{NSObject, NSPoint, NSRect, NSSize};

define_class!(
    /// The strip over the frame's top; it owns the content's tracking of the pointer.
    #[unsafe(super(NSVisualEffectView, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "NsxivNgTitleStrip"]
    #[ivars = Cell<bool>]
    pub(crate) struct TitleStrip;

    impl TitleStrip {
        // a drag on the strip moves the window, as the title bar would
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn can_move_window(&self) -> bool {
            true
        }

        // a press on the strip is not the frame's, which it covers
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _: &NSEvent) {}

        // a move anywhere over the content shows the bars; outside the strip they go again
        // half a second after the last
        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            // SAFETY: NSObject's own, which cancels this strip's hide: asked for below
            unsafe {
                let _: () = msg_send![
                    NSObject::class(),
                    cancelPreviousPerformRequestsWithTarget: self,
                    selector: sel!(hide:),
                    object: None::<&AnyObject>
                ];
            }
            self.fade(1.0);
            let at = self.convertPoint_fromView(event.locationInWindow(), None);
            let size = self.bounds().size;
            let inside = (0.0..size.width).contains(&at.x) && (0.0..size.height).contains(&at.y);
            if !inside {
                // SAFETY: NSObject's own, with this strip's method that takes an object
                unsafe {
                    let _: () = msg_send![
                        self,
                        performSelector: sel!(hide:),
                        withObject: None::<&AnyObject>,
                        afterDelay: 0.5f64
                    ];
                }
            }
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _: &NSEvent) {
            self.fade(0.0);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            if event.clickCount() == 2
                && let Some(window) = self.window()
            {
                window.zoom(None);
            }
        }

        #[unsafe(method(hide:))]
        fn hide(&self, _: Option<&AnyObject>) {
            self.fade(0.0);
        }
    }
);

impl TitleStrip {
    /// Makes the window's frame fill it, under a transparent title bar, and puts the strip
    /// over `view`'s top, the bars invisible.
    pub(crate) fn install(window: &NSWindow, view: &NSView) -> Retained<TitleStrip> {
        let mtm = MainThreadMarker::from(window);
        window.setTitlebarAppearsTransparent(true);
        window.setStyleMask(window.styleMask() | NSWindowStyleMask::FullSizeContentView);
        let titled = NSWindow::frameRectForContentRect_styleMask(
            NSRect::ZERO,
            NSWindowStyleMask::Titled,
            mtm,
        );
        let height = titled.size.height;
        let bounds = view.bounds();
        let frame = NSRect::new(
            NSPoint::new(0.0, bounds.size.height - height),
            NSSize::new(bounds.size.width, height),
        );
        let this = Self::alloc(mtm).set_ivars(Cell::new(false));
        // SAFETY: NSView's designated initializer, on a view just allocated
        let strip: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        strip.setMaterial(NSVisualEffectMaterial::Titlebar);
        strip.setBlendingMode(NSVisualEffectBlendingMode::WithinWindow);
        strip.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewMinYMargin,
        );
        strip.setAlphaValue(0.0);
        view.addSubview_positioned_relativeTo(&strip, NSWindowOrderingMode::Above, None);
        let options = NSTrackingAreaOptions::ActiveAlways
            | NSTrackingAreaOptions::MouseMoved
            | NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::InVisibleRect;
        // SAFETY: an area of the view's visible rectangle, which the strip owns, with no
        // user info
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                options,
                Some(&strip),
                None,
            )
        };
        view.addTrackingArea(&area);
        if let Some(bar) = strip.system_bar() {
            bar.setAlphaValue(0.0);
        }
        strip
    }

    /// The window went into fullscreen or out of it, which no move of the pointer tells: there
    /// the system's bar is opaque and the strip invisible, and out of it both are invisible.
    pub(crate) fn fullscreen(&self, fullscreen: bool) {
        self.ivars().set(fullscreen);
        self.fade(0.0);
    }

    // The system's view of the title bar: its buttons, the title and the file's icon.
    fn system_bar(&self) -> Option<Retained<NSView>> {
        let button = self
            .window()?
            .standardWindowButton(NSWindowButton::CloseButton)?;
        // SAFETY: the view the button is in, which the window keeps
        unsafe { button.superview() }
    }

    fn fade(&self, alpha: f64) {
        let fullscreen = self.ivars().get()
            || self
                .window()
                .is_some_and(|w| w.styleMask().contains(NSWindowStyleMask::FullScreen));
        let bar = self.system_bar();
        let changes = StackBlock::new(|context: NonNull<NSAnimationContext>| {
            // SAFETY: the context AppKit passes, alive for the call
            unsafe { context.as_ref() }.setDuration(0.2);
            if let Some(bar) = &bar {
                bar.animator()
                    .setAlphaValue(if fullscreen { 1.0 } else { alpha });
            }
            let strip: &NSView = self;
            strip
                .animator()
                .setAlphaValue(if fullscreen { 0.0 } else { alpha });
        });
        NSAnimationContext::runAnimationGroup(&changes);
    }
}
