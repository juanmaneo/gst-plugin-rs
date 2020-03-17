// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use futures::future::BoxFuture;
use futures::future::{abortable, AbortHandle};
use futures::prelude::*;

use glib;
use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use glib::{glib_object_impl, glib_object_subclass};

use gst;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error_msg, gst_log, gst_trace};

use lazy_static::lazy_static;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::u32;

use crate::runtime::prelude::*;
use crate::runtime::{self, Context, PadSink, PadSinkRef, PadSrc, PadSrcRef};
use crate::get_current_running_time;

const DEFAULT_CONTEXT: &str = "";
const DEFAULT_CONTEXT_WAIT: u32 = 0;

#[derive(Debug, Clone)]
struct Settings {
    context: String,
    context_wait: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            context: DEFAULT_CONTEXT.into(),
            context_wait: DEFAULT_CONTEXT_WAIT,
        }
    }
}

static PROPERTIES: [subclass::Property; 3] = [
    subclass::Property("context", |name| {
        glib::ParamSpec::string(
            name,
            "Context",
            "Context name to share threads with",
            Some(DEFAULT_CONTEXT),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("context-wait", |name| {
        glib::ParamSpec::uint(
            name,
            "Context Wait",
            "Throttle poll loop to run at most once every this many ms",
            0,
            1000,
            DEFAULT_CONTEXT_WAIT,
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("active-pad", |name| {
        glib::ParamSpec::object(
            name,
            "Active Pad",
            "Currently active pad",
            gst::Pad::static_type(),
            glib::ParamFlags::READWRITE,
        )
    }),
];

#[derive(Debug)]
struct InputSelectorPadSinkHandlerInner {
    segment: Option<gst::Segment>,
    send_sticky: bool,
    abort_handle: Option<AbortHandle>,
}

impl Default for InputSelectorPadSinkHandlerInner {
    fn default() -> Self {
        InputSelectorPadSinkHandlerInner {
            segment: None,
            send_sticky: true,
            abort_handle: None,
        }
    }
}

#[derive(Clone, Debug)]
struct InputSelectorPadSinkHandler(Arc<Mutex<InputSelectorPadSinkHandlerInner>>);

impl InputSelectorPadSinkHandler {
    fn new() -> Self {
        InputSelectorPadSinkHandler(Arc::new(Mutex::new(
            InputSelectorPadSinkHandlerInner::default(),
        )))
    }

    /* Wait until specified time */
    async fn sync(&self, element: &gst::Element, running_time: gst::ClockTime) {
        let now = get_current_running_time(&element);

        if now.is_some() && now < running_time {
            let delay = running_time - now;
            runtime::time::delay_for(Duration::from_nanos(delay.nseconds().unwrap())).await;
        }
    }

    async fn handle_item(
        &self,
        pad: &PadSinkRef<'_>,
        element: &gst::Element,
        mut buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let inputselector = InputSelector::from_instance(element);

        let (stickies, is_active, sync_future, switched_pad) = {
            let mut state = inputselector.state.lock().unwrap();
            let mut inner = self.0.lock().unwrap();
            let mut stickies = vec![];
            let mut sync_future = None;
            let switched_pad = state.switched_pad;

            if let Some(segment) = &inner.segment {
                if let Some(segment) = segment.downcast_ref::<gst::format::Time>() {
                    let rtime = segment.to_running_time(buffer.get_pts());
                    let (sync_fut, abort_handle) = abortable(self.sync(&element, rtime));
                    inner.abort_handle = Some(abort_handle);
                    sync_future = Some(sync_fut.map_err(|_| gst::FlowError::Flushing));
                }
            }

            let is_active = {
                if state.active_sinkpad.as_ref() == Some(pad.gst_pad()) {
                    if inner.send_sticky || state.switched_pad {
                        pad.gst_pad().sticky_events_foreach(|event| {
                            stickies.push(event.clone());
                            Ok(Some(event))
                        });

                        inner.send_sticky = false;
                        state.switched_pad = false;
                    }
                    true
                } else {
                    false
                }
            };

            (stickies, is_active, sync_future, switched_pad)
        };

        if let Some(sync_fut) = sync_future {
            sync_fut.await?;
        }

        for event in stickies {
            inputselector.src_pad.push_event(event).await;
        }

        if is_active {
            gst_log!(CAT, obj: pad.gst_pad(), "Forwarding {:?}", buffer);

            if switched_pad && !buffer.get_flags().contains(gst::BufferFlags::DISCONT) {
                let buffer = buffer.make_mut();
                buffer.set_flags(gst::BufferFlags::DISCONT);
            }

            inputselector.src_pad.push(buffer).await
        } else {
            Ok(gst::FlowSuccess::Ok)
        }
    }
}

impl PadSinkHandler for InputSelectorPadSinkHandler {
    type ElementImpl = InputSelector;

    fn sink_chain(
        &self,
        pad: &PadSinkRef,
        _inputselector: &InputSelector,
        element: &gst::Element,
        buffer: gst::Buffer,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let this = self.clone();
        let element = element.clone();
        let pad_weak = pad.downgrade();
        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            this.handle_item(&pad, &element, buffer).await
        }
        .boxed()
    }

    fn sink_chain_list(
        &self,
        pad: &PadSinkRef,
        _inputselector: &InputSelector,
        element: &gst::Element,
        list: gst::BufferList,
    ) -> BoxFuture<'static, Result<gst::FlowSuccess, gst::FlowError>> {
        let this = self.clone();
        let element = element.clone();
        let pad_weak = pad.downgrade();
        async move {
            let pad = pad_weak.upgrade().expect("PadSink no longer exists");
            gst_log!(CAT, obj: pad.gst_pad(), "Handling buffer list {:?}", list);
            // TODO: Ideally we would keep the list intact and forward it in one go
            for buffer in list.iter_owned() {
                this.handle_item(&pad, &element, buffer).await?;
            }

            Ok(gst::FlowSuccess::Ok)
        }
        .boxed()
    }

    fn sink_event_serialized(
        &self,
        _pad: &PadSinkRef,
        _inputselector: &InputSelector,
        _element: &gst::Element,
        event: gst::Event,
    ) -> BoxFuture<'static, bool> {
        let this = self.clone();

        async move {
            let mut inner = this.0.lock().unwrap();

            // Remember the segment for later use
            match event.view() {
                gst::EventView::Segment(e) => {
                    inner.segment = Some(e.get_segment().clone());
                }
                _ => (),
            }

            // We sent sticky events together with the next buffer once it becomes
            // the active pad.
            //
            // TODO: Other serialized events for the active pad can also be forwarded
            // here, and sticky events could be forwarded directly. Needs forwarding of
            // all other sticky events first!
            if event.is_sticky() {
                inner.send_sticky = true;
                true
            } else {
                true
            }
        }
        .boxed()
    }

    fn sink_event(
        &self,
        _pad: &PadSinkRef,
        inputselector: &InputSelector,
        _element: &gst::Element,
        event: gst::Event,
    ) -> bool {
        /* Drop all events for now */
        match event.view() {
            gst::EventView::FlushStart(..) => {
                /* Unblock downstream */
                inputselector.src_pad.gst_pad().push_event(event.clone());

                let mut inner = self.0.lock().unwrap();

                if let Some(abort_handle) = inner.abort_handle.take() {
                    abort_handle.abort();
                }
            }
            _ => (),
        }
        true
    }

    fn sink_query(
        &self,
        pad: &PadSinkRef,
        inputselector: &InputSelector,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        gst_log!(CAT, obj: pad.gst_pad(), "Handling query {:?}", query);

        if query.is_serialized() {
            // FIXME: How can we do this (drops ALLOCATION and DRAIN)?
            gst_log!(CAT, obj: pad.gst_pad(), "Dropping serialized query {:?}", query);
            false
        } else {
            gst_log!(CAT, obj: pad.gst_pad(), "Forwarding query {:?}", query);
            inputselector.src_pad.gst_pad().peer_query(query)
        }
    }
}

#[derive(Clone, Debug)]
struct InputSelectorPadSrcHandler;

impl InputSelectorPadSrcHandler {}

impl PadSrcHandler for InputSelectorPadSrcHandler {
    type ElementImpl = InputSelector;

    fn src_query(
        &self,
        pad: &PadSrcRef,
        inputselector: &InputSelector,
        _element: &gst::Element,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad.gst_pad(), "Handling {:?}", query);

        match query.view_mut() {
            QueryView::Latency(ref mut q) => {
                let mut ret = true;
                let mut min_latency: gst::ClockTime = 0.into();
                let mut max_latency: gst::ClockTime = 0.into();
                let sinkpad = {
                    let state = inputselector.state.lock().unwrap();
                    state.active_sinkpad.clone()
                };

                if let Some(sinkpad) = sinkpad {
                    let mut peer_query = gst::query::Query::new_latency();

                    ret = sinkpad.peer_query(&mut peer_query);

                    if ret {
                        let (_, min, max) = peer_query.get_result();
                        min_latency = min;
                        max_latency = max;
                    }
                }

                q.set(true, min_latency, max_latency);

                ret
            }
            _ => {
                let sinkpad = {
                    let state = inputselector.state.lock().unwrap();
                    state.active_sinkpad.clone()
                };

                if let Some(sinkpad) = sinkpad {
                    sinkpad.peer_query(query)
                } else {
                    true
                }
            }
        }
    }
}

#[derive(Debug)]
struct State {
    active_sinkpad: Option<gst::Pad>,
    switched_pad: bool,
}

impl Default for State {
    fn default() -> State {
        State {
            active_sinkpad: None,
            switched_pad: true,
        }
    }
}

#[derive(Debug)]
struct Pads {
    pad_serial: u32,
    sink_pads: HashMap<gst::Pad, PadSink>,
}

impl Default for Pads {
    fn default() -> Pads {
        Pads {
            pad_serial: 0,
            sink_pads: HashMap::new(),
        }
    }
}

#[derive(Debug)]
struct InputSelector {
    src_pad: PadSrc,
    state: Mutex<State>,
    settings: Mutex<Settings>,
    pads: Mutex<Pads>,
}

lazy_static! {
    static ref CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-input-selector",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing input selector"),
    );
}

impl InputSelector {
    fn prepare(&self, element: &gst::Element) -> Result<(), gst::ErrorMessage> {
        gst_debug!(CAT, obj: element, "Preparing");

        let settings = self.settings.lock().unwrap();

        let context =
            Context::acquire(&settings.context, settings.context_wait).map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to acquire Context: {}", err]
                )
            })?;

        self.src_pad
            .prepare(context, &InputSelectorPadSrcHandler {})
            .map_err(|err| {
                gst_error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Error joining Context: {:?}", err]
                )
            })?;

        gst_debug!(CAT, obj: element, "Prepared");

        Ok(())
    }

    fn unprepare(&self, element: &gst::Element) -> Result<(), ()> {
        let mut state = self.state.lock().unwrap();
        gst_debug!(CAT, obj: element, "Unpreparing");

        let _ = self.src_pad.unprepare();

        *state = State::default();

        gst_debug!(CAT, obj: element, "Unprepared");

        Ok(())
    }
}

impl ObjectSubclass for InputSelector {
    const NAME: &'static str = "RsTsInputSelector";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Thread-sharing input selector",
            "Generic",
            "Simple input selector element",
            "Mathieu Duponchelle <mathieu@centricular.com>",
        );

        let caps = gst::Caps::new_any();

        let sink_pad_template = gst::PadTemplate::new(
            "sink_%u",
            gst::PadDirection::Sink,
            gst::PadPresence::Request,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);

        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        klass.install_properties(&PROPERTIES);
    }

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("src").unwrap();
        let src_pad = PadSrc::new_from_template(&templ, Some("src"));

        Self {
            src_pad,
            state: Mutex::new(State::default()),
            settings: Mutex::new(Settings::default()),
            pads: Mutex::new(Pads::default()),
        }
    }
}

impl ObjectImpl for InputSelector {
    glib_object_impl!();

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("context", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context = value
                    .get()
                    .expect("type checked upstream")
                    .unwrap_or_else(|| "".into());
            }
            subclass::Property("context-wait", ..) => {
                let mut settings = self.settings.lock().unwrap();
                settings.context_wait = value.get_some().expect("type checked upstream");
            }
            subclass::Property("active-pad", ..) => {
                let pad = value.get::<gst::Pad>().expect("type checked upstream");
                let mut state = self.state.lock().unwrap();
                let pads = self.pads.lock().unwrap();
                if let Some(pad) = pad {
                    if pads.sink_pads.get(&pad).is_some() {
                        state.active_sinkpad = Some(pad);
                        state.switched_pad = true;
                    }
                } else {
                    state.active_sinkpad = None;
                }
            }
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("context", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.context.to_value())
            }
            subclass::Property("context-wait", ..) => {
                let settings = self.settings.lock().unwrap();
                Ok(settings.context_wait.to_value())
            }
            subclass::Property("active-pad", ..) => {
                let state = self.state.lock().unwrap();
                let active_pad = state.active_sinkpad.clone();
                Ok(active_pad.to_value())
            }
            _ => unimplemented!(),
        }
    }

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(self.src_pad.gst_pad()).unwrap();
    }
}

impl ElementImpl for InputSelector {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_trace!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                self.prepare(element).map_err(|err| {
                    element.post_error_message(&err);
                    gst::StateChangeError
                })?;
            }
            gst::StateChange::ReadyToNull => {
                self.unprepare(element).map_err(|_| gst::StateChangeError)?;
            }
            _ => (),
        }

        let mut success = self.parent_change_state(element, transition)?;

        match transition {
            gst::StateChange::ReadyToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            gst::StateChange::PlayingToPaused => {
                success = gst::StateChangeSuccess::NoPreroll;
            }
            _ => (),
        }

        Ok(success)
    }

    fn request_new_pad(
        &self,
        element: &gst::Element,
        templ: &gst::PadTemplate,
        _name: Option<String>,
        _caps: Option<&gst::Caps>,
    ) -> Option<gst::Pad> {
        let mut state = self.state.lock().unwrap();
        let mut pads = self.pads.lock().unwrap();
        let sink_pad =
            gst::Pad::new_from_template(&templ, Some(format!("sink_{}", pads.pad_serial).as_str()));
        pads.pad_serial += 1;
        sink_pad.set_active(true).unwrap();
        element.add_pad(&sink_pad).unwrap();
        let sink_pad = PadSink::new(sink_pad);
        let ret = sink_pad.gst_pad().clone();

        sink_pad.prepare(&InputSelectorPadSinkHandler::new());

        if state.active_sinkpad.is_none() {
            state.active_sinkpad = Some(ret.clone());
            state.switched_pad = true;
        }

        pads.sink_pads.insert(ret.clone(), sink_pad);

        Some(ret)
    }

    fn release_pad(&self, element: &gst::Element, pad: &gst::Pad) {
        let mut pads = self.pads.lock().unwrap();
        let sink_pad = pads.sink_pads.remove(pad).unwrap();
        sink_pad.unprepare();
        element.remove_pad(pad).unwrap();
    }
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "ts-input-selector",
        gst::Rank::None,
        InputSelector::get_type(),
    )
}
