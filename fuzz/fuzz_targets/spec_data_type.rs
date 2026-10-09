//! The reader of channel data types never panics, and a data type prints as the text
//! it was read from.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use libfuzzer_sys::fuzz_target;
use spec::data_type::DataType;

fuzz_target!(|text: &str| {
    if let Ok(data_type) = text.parse::<DataType>() {
        assert_eq!(data_type.to_string(), text, "the data type changed");
    }
});
