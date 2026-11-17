#[cfg(not(target_os = "macos"))]
fn main() {}

#[cfg(target_os = "macos")]
fn main() {
    native::run();
}

#[cfg(target_os = "macos")]
mod native {
    use gpui::{
        App, AppContext, Application, Context, FocusHandle, IntoElement, QuitMode, Render, Window,
        WindowOptions, div, prelude::*,
    };
    use gpui_macos::MacPlatform;
    use objc2::{
        MainThreadMarker, class, msg_send,
        rc::{Retained, Weak, autoreleasepool},
        runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel},
        sel,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString, NSUInteger};
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use std::{cell::Cell, ffi::c_void, ptr, rc::Rc, time::Duration};

    thread_local! {
        static OBSERVATIONS: Cell<usize> = const { Cell::new(0) };
    }

    struct Prompt {
        focus: FocusHandle,
    }

    impl Render for Prompt {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("upload")
                .track_focus(&self.focus)
                .role(gpui::Role::Button)
                .aria_label("Upload")
                .size_full()
        }
    }

    // Query from an Objective-C callback as well as directly from the test. A
    // notification can synchronously re-enter accessibility while AppKit works.
    extern "C" fn observed_change(
        _: &AnyObject,
        _: Sel,
        _: *mut AnyObject,
        view: *mut AnyObject,
        _: *mut AnyObject,
        _: *mut c_void,
    ) {
        OBSERVATIONS.with(|count| count.set(count.get() + 1));
        unsafe {
            let _: *mut AnyObject = msg_send![view, accessibilityChildren];
        }
    }

    fn query(view: &AnyObject) -> usize {
        unsafe {
            let children: *mut AnyObject = msg_send![view, accessibilityChildren];
            let _: *mut AnyObject = msg_send![view, accessibilityFocusedUIElement];
            let _: *mut AnyObject = msg_send![view, accessibilityHitTest: NSPoint::new(0., 0.)];
            if children.is_null() {
                0
            } else {
                let count: NSUInteger = msg_send![children, count];
                count
            }
        }
    }

    pub fn run() {
        assert!(MainThreadMarker::new().is_some());
        let disabled = std::env::args().any(|arg| arg == "--disabled");
        let platform = Rc::new(MacPlatform::new(false));
        let application = if disabled {
            Application::new_inaccessible(platform)
        } else {
            Application::with_platform(platform)
        };
        application.with_quit_mode(QuitMode::Explicit).run(move |cx: &mut App| {
            autoreleasepool(|_| unsafe {
                // AppKit can query the class before MacWindow attaches its state.
                let class = AnyClass::get(c"GPUIView").expect("registered GPUIView");
                let view: *mut AnyObject = msg_send![class, alloc];
                let view: Retained<AnyObject> = msg_send![view,
                    initWithFrame: NSRect::new(NSPoint::new(0., 0.), NSSize::new(100., 100.))];
                query(&view);
            });
            cx.spawn(async move |cx| {
                let observer: Retained<AnyObject> = unsafe {
                    let mut builder = ClassBuilder::new(c"GPUIAccessibilityKvoObserver", class!(NSObject))
                        .expect("unique observer class");
                    builder.add_method(
                        sel!(observeValueForKeyPath:ofObject:change:context:),
                        observed_change as extern "C" fn(_, _, _, _, _, _),
                    );
                    msg_send![builder.register(), new]
                };
                let key = NSString::from_str("hidden");
                for iteration in 0..20 {
                    let (handle, view, content) = autoreleasepool(|_| cx.update(|cx| {
                        let handle = cx.open_window(WindowOptions {
                            show: false,
                            focus: false,
                            ..Default::default()
                        }, |window, cx| cx.new(|cx| {
                            let focus = cx.focus_handle();
                            window.focus(&focus, cx);
                            Prompt { focus }
                        })).expect("open native window");
                        let (view, content) = handle.update(cx, |_, window, _| unsafe {
                            let RawWindowHandle::AppKit(raw) = HasWindowHandle::window_handle(window)
                                .expect("native handle").as_raw() else { panic!("AppKit handle") };
                            let view = Retained::retain(raw.ns_view.as_ptr().cast::<AnyObject>())
                                .expect("retained GPUIView");
                            let native_window: *mut AnyObject = msg_send![&*view, window];
                            let content: Retained<AnyObject> = msg_send![native_window, contentView];
                            (view, content)
                        }).expect("read native window");
                        (handle, view, content)
                    }));

                    // Exercise both an adapter that was never queried and one
                    // activated while App is borrowed, as real clients can do.
                    if iteration % 2 == 0 {
                        autoreleasepool(|_| cx.update(|_| {
                            if !disabled {
                                assert!(query(&view) > 0, "initial accessibility tree");
                            } else {
                                query(&view);
                            }
                        }));
                        cx.background_executor().timer(Duration::from_millis(20)).await;
                        autoreleasepool(|_| cx.update(|cx| {
                            handle.update(cx, |_, window, cx| {
                                window.refresh();
                                window.draw(cx);
                                assert_eq!(window.is_a11y_active(), !disabled);
                            }).expect("draw activated tree");
                        }));
                    }

                    // The old adapter subclasses contentView, which can differ
                    // from GPUIView. Observe both to catch either attachment.
                    let mut views = vec![view];
                    if !ptr::eq(&*views[0], &*content) {
                        views.push(content);
                    } else {
                        drop(content);
                    }
                    let classes: Vec<_> = autoreleasepool(|_| views.iter().map(|view| unsafe {
                        let _: () = msg_send![&**view, addObserver: &*observer,
                            forKeyPath: &*key, options: 0usize, context: ptr::null_mut::<c_void>()];
                        view.class()
                    }).collect());
                    autoreleasepool(|_| cx.update(|cx| {
                        handle.update(cx, |_, window, _| window.remove_window())
                            .expect("close GPUI window");
                    }));
                    let weak_views = autoreleasepool(|_| views.iter().zip(classes).map(|(view, class)| unsafe {
                        assert_eq!(view.class(), class, "closing must preserve the KVO subclass");
                        query(view);
                        let before = OBSERVATIONS.with(Cell::get);
                        let hidden: Bool = msg_send![&**view, isHidden];
                        let _: () = msg_send![&**view, setHidden: Bool::new(!hidden.as_bool())];
                        assert!(OBSERVATIONS.with(Cell::get) > before, "KVO still delivers after close");
                        let _: () = msg_send![&**view, removeObserver: &*observer, forKeyPath: &*key];
                        Weak::from_retained(view)
                    }).collect::<Vec<_>>());
                    drop(views);
                    for _ in 0..100 {
                        cx.background_executor().timer(Duration::from_millis(20)).await;
                        if weak_views.iter().all(|view| view.load().is_none()) {
                            break;
                        }
                    }
                    assert!(weak_views.iter().all(|view| view.load().is_none()), "closed views must be released");
                }
                println!("accessibility lifecycle: 20 native windows passed (disabled={disabled})");
                cx.update(|cx| cx.quit());
            }).detach();
        });
    }
}
