// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fmt::Error as FmtError;

use glib_ffi;
use gst_ffi;

use glib;
use glib::translate::ToGlibPtr;
use gst;
use gst::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlowError {
    Flushing,
    Eos,
    NotNegotiated(gst::ErrorMessage),
    Error(gst::ErrorMessage),
}

impl Into<gst::FlowReturn> for FlowError {
    fn into(self) -> gst::FlowReturn {
        (&self).into()
    }
}

impl<'a> Into<gst::FlowReturn> for &'a FlowError {
    fn into(self) -> gst::FlowReturn {
        match *self {
            FlowError::Flushing => gst::FlowReturn::Flushing,
            FlowError::Eos => gst::FlowReturn::Eos,
            FlowError::NotNegotiated(..) => gst::FlowReturn::NotNegotiated,
            FlowError::Error(..) => gst::FlowReturn::Error,
        }
    }
}

impl Display for FlowError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), FmtError> {
        match *self {
            FlowError::Flushing | FlowError::Eos => f.write_str(self.description()),
            FlowError::NotNegotiated(ref m) => {
                f.write_fmt(format_args!("{}: {}", self.description(), m))
            }
            FlowError::Error(ref m) => f.write_fmt(format_args!("{}: {}", self.description(), m)),
        }
    }
}

impl Error for FlowError {
    fn description(&self) -> &str {
        match *self {
            FlowError::Flushing => "Flushing",
            FlowError::Eos => "Eos",
            FlowError::NotNegotiated(..) => "Not Negotiated",
            FlowError::Error(..) => "Error",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct UriError {
    error: gst::URIError,
    message: String,
}

impl UriError {
    pub fn new<T: Into<String>>(error: gst::URIError, message: T) -> UriError {
        UriError {
            error: error,
            message: message.into(),
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn error(&self) -> gst::URIError {
        self.error
    }
}

impl Into<glib::Error> for UriError {
    fn into(self) -> glib::Error {
        (&self).into()
    }
}

impl<'a> Into<glib::Error> for &'a UriError {
    fn into(self) -> glib::Error {
        glib::Error::new(self.error, &self.message)
    }
}

impl Display for UriError {
    fn fmt(&self, f: &mut Formatter) -> Result<(), FmtError> {
        f.write_fmt(format_args!("{}: {}", self.description(), self.message))
    }
}

impl Error for UriError {
    fn description(&self) -> &str {
        match self.error {
            gst::URIError::UnsupportedProtocol => "Unsupported protocol",
            gst::URIError::BadUri => "Bad URI",
            gst::URIError::BadState => "Bad State",
            gst::URIError::BadReference => "Bad Reference",
            _ => "Unknown",
        }
    }
}

#[macro_export]
macro_rules! panic_to_error(
    ($element:expr, $panicked:expr, $ret:expr, $code:block) => {{
        use std::panic::{self, AssertUnwindSafe};
        use std::sync::atomic::Ordering;

        if $panicked.load(Ordering::Relaxed) {
            $element.post_error_message(&gst_error_msg!(gst::LibraryError::Failed, ["Panicked"]));
            $ret
        } else {
            let result = panic::catch_unwind(AssertUnwindSafe(|| $code));

            match result {
                Ok(result) => result,
                Err(err) => {
                    $panicked.store(true, Ordering::Relaxed);
                    if let Some(cause) = err.downcast_ref::<&str>() {
                        $element.post_error_message(&gst_error_msg!(gst::LibraryError::Failed, ["Panicked: {}", cause]));
                    } else if let Some(cause) = err.downcast_ref::<String>() {
                        $element.post_error_message(&gst_error_msg!(gst::LibraryError::Failed, ["Panicked: {}", cause]));
                    } else {
                        $element.post_error_message(&gst_error_msg!(gst::LibraryError::Failed, ["Panicked"]));
                    }
                    $ret
                }
            }
        }
    }};
);
