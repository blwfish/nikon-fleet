//! The one definition of each JSON value shape `sdk::decode_value` produces, and of when two
//! range values are "the same".
//!
//! Transplant, restore, apply and diff all need to ask "is this an enum / a range / a stepped
//! range?" and "did this range land where it was meant to?". They used to answer each question
//! with their own key test and their own tolerance, which disagreed at the margins (a degenerate
//! range, a missing `steps`, a relative tolerance that exceeded one step near the top of a scale).
//! Every such question is answered here, once.

use serde_json::Value;

/// An enum shape: carries `values` and, unlike a range, no `lower`.
pub(crate) fn is_enum_shape(value: &Value) -> bool {
    value.is_object() && value.get("values").is_some() && value.get("lower").is_none()
}

/// A range shape, by its keys: `lower` and `upper` are present. Whether their values are usable
/// is [`range_scale`]'s question; a range-shaped value that fails it is malformed, not a scalar.
pub(crate) fn is_range_shape(value: &Value) -> bool {
    value.is_object() && value.get("lower").is_some() && value.get("upper").is_some()
}

/// `lower`, `upper` and `steps` of a range-shaped value, or `None` if any is missing or not a
/// number of the right kind (`null` included).
pub(crate) fn range_scale(value: &Value) -> Option<(f64, f64, u64)> {
    Some((value.get("lower")?.as_f64()?, value.get("upper")?.as_f64()?, value.get("steps")?.as_u64()?))
}

/// A range with discrete positions: at least two steps over a span. The SDK leaves `value` at 0.0
/// on these and reports the position in `value_index` (docs/mcp-hardware-findings.md F18).
pub(crate) fn is_stepped_range(value: &Value) -> bool {
    range_scale(value).is_some_and(|(lower, upper, steps)| steps >= 2 && upper != lower)
}

/// A range that holds no choice: the same number at both ends. A lens read-out such as
/// FocalLength (0..0, 0 steps) looks like this; there is nothing to set.
pub(crate) fn has_no_span(value: &Value) -> bool {
    range_scale(value).is_some_and(|(lower, upper, _)| upper == lower)
}

/// What a stepped range holds at position `index`.
pub(crate) fn value_at(lower: f64, upper: f64, steps: u64, index: u64) -> f64 {
    lower + index as f64 * (upper - lower) / (steps as f64 - 1.0)
}

/// The number a range value actually holds: a stepped range's from its `value_index`, a
/// continuous range's from `value`. A stepped range with no usable index, or an index past its
/// last step, has no number: reading its junk `value` (0.0) would put it at the bottom of the
/// scale without saying so.
pub(crate) fn range_value(v: &Value) -> Option<f64> {
    if is_stepped_range(v) {
        let (lower, upper, steps) = range_scale(v)?;
        let index = v.get("value_index")?.as_u64()?;
        return (index < steps).then(|| value_at(lower, upper, steps, index));
    }
    v.get("value").and_then(Value::as_f64)
}

/// Half a step of a stepped range: the most two readings of one position can differ by, and
/// less than the gap to the next position.
fn half_step(v: &Value) -> Option<f64> {
    let (lower, upper, steps) = range_scale(v)?;
    (steps >= 2 && upper != lower).then(|| (upper - lower).abs() / (steps as f64 - 1.0) / 2.0)
}

/// Do two range values hold the same position (stepped) or the same number to float precision
/// (continuous)? The tolerance is derived from the scale, never from the magnitude: a relative
/// tolerance exceeded one step near the top of WBTune* (0..1224, 1 per step) and let a
/// one-step miss read as a landed write.
pub(crate) fn range_values_agree(a: &Value, b: &Value) -> bool {
    let (Some(x), Some(y)) = (range_value(a), range_value(b)) else { return false };
    let tolerance = match (half_step(a), half_step(b)) {
        (Some(p), Some(q)) => p.min(q),
        (Some(p), None) | (None, Some(p)) => p,
        (None, None) => 1e-6_f64.max(x.abs() * 1e-9),
    };
    (x - y).abs() < tolerance
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn stepped(lower: f64, upper: f64, steps: u64, index: u64) -> Value {
        // `value` is 0.0 on every stepped range the SDK reports (F18).
        json!({"value": 0.0, "default": 0.0, "value_index": index, "default_index": 0, "lower": lower, "upper": upper, "steps": steps})
    }

    fn continuous(lower: f64, upper: f64, value: f64) -> Value {
        json!({"value": value, "default": 0.0, "value_index": 0, "default_index": 0, "lower": lower, "upper": upper, "steps": 0})
    }

    #[test]
    fn shapes_are_distinguished_by_their_keys() {
        assert!(is_enum_shape(&json!({"values": [1, 2], "value_index": 0})));
        assert!(!is_enum_shape(&stepped(0.0, 10.0, 11, 3)));
        assert!(is_range_shape(&stepped(0.0, 10.0, 11, 3)));
        assert!(!is_range_shape(&json!({"values": []})));
        assert!(!is_range_shape(&json!(5)));
        assert!(!is_enum_shape(&json!("Standard")));
    }

    #[test]
    fn stepped_needs_two_steps_and_a_span() {
        assert!(is_stepped_range(&stepped(0.0, 1224.0, 1225, 0)));
        assert!(is_stepped_range(&stepped(0.0, 1.0, 2, 0)), "two steps is the least that is stepped");
        assert!(!is_stepped_range(&stepped(0.0, 1.0, 1, 0)), "one step is not");
        assert!(!is_stepped_range(&stepped(0.0, 1.0, 0, 0)), "continuous");
        assert!(!is_stepped_range(&stepped(5.0, 5.0, 10, 0)), "no span");
        assert!(!is_stepped_range(&json!({"lower": 0.0, "upper": 1.0, "steps": null})), "null steps is not a number");
        assert!(!is_stepped_range(&json!({"lower": null, "upper": 1.0, "steps": 5})));
    }

    #[test]
    fn a_range_with_the_same_number_at_both_ends_has_no_span() {
        assert!(has_no_span(&continuous(0.0, 0.0, 0.0)), "FocalLength on all three bodies");
        assert!(!has_no_span(&continuous(0.0, 1.0, 0.0)));
        assert!(!has_no_span(&json!({"lower": 0.0, "upper": 0.0})), "no steps: not judged");
    }

    #[test]
    fn a_stepped_ranges_number_comes_from_its_index_not_its_junk_value() {
        assert_eq!(range_value(&stepped(0.0, 1224.0, 1225, 1224)), Some(1224.0));
        assert_eq!(range_value(&stepped(2500.0, 10000.0, 751, 750)), Some(10000.0));
        assert_eq!(range_value(&continuous(0.0, 10.0, 3.5)), Some(3.5));
    }

    #[test]
    fn a_stepped_range_without_a_usable_index_has_no_number() {
        let mut no_index = stepped(0.0, 10.0, 11, 0);
        no_index.as_object_mut().unwrap().remove("value_index");
        assert_eq!(range_value(&no_index), None);
        assert_eq!(range_value(&stepped(0.0, 10.0, 11, 11)), None, "index == steps is one past the end");
        assert_eq!(range_value(&stepped(0.0, 10.0, 11, 10)), Some(10.0), "the last index is valid");
    }

    #[test]
    fn one_step_apart_is_never_the_same_position() {
        // At, below and above the old relative tolerance, on the scales that exposed it.
        for (lower, upper, steps) in [(0.0, 1224.0, 1225), (2500.0, 10000.0, 751), (1.0, 32767.0, 32767)] {
            for index in [0, 1, steps / 2, steps - 2] {
                let here = stepped(lower, upper, steps, index);
                assert!(range_values_agree(&here, &here.clone()), "{lower}..{upper} @ {index}");
                let next = stepped(lower, upper, steps, index + 1);
                assert!(!range_values_agree(&here, &next), "{lower}..{upper}: {index} vs {}", index + 1);
                assert!(!range_values_agree(&next, &here));
            }
        }
    }

    #[test]
    fn the_top_of_the_scale_is_where_the_relative_tolerance_failed() {
        let top = stepped(0.0, 1224.0, 1225, 1224);
        assert!(!range_values_agree(&top, &stepped(0.0, 1224.0, 1225, 1223)));
        let kelvin_top = stepped(2500.0, 10000.0, 751, 750);
        assert!(!range_values_agree(&kelvin_top, &stepped(2500.0, 10000.0, 751, 749)), "10000 K vs 9990 K");
    }

    #[test]
    fn two_ranges_on_different_scales_agree_only_to_the_finer_one() {
        // 5.0 and 5.1 are the same position on an 11-step scale over 0..10 (step 1: 5.1 snaps to 5),
        // but two different positions on a 101-step one (step 0.1). Comparing them must use the finer
        // resolution, or a coarse side makes a real difference vanish.
        let coarse = stepped(0.0, 10.0, 11, 5);
        let fine_5_0 = stepped(0.0, 10.0, 101, 50);
        let fine_5_1 = stepped(0.0, 10.0, 101, 51);
        assert!(range_values_agree(&coarse, &fine_5_0), "the same number");
        assert!(!range_values_agree(&coarse, &fine_5_1), "5.0 vs 5.1 differ at the finer scale");
        assert!(!range_values_agree(&fine_5_1, &coarse), "symmetric");
    }

    #[test]
    fn a_continuous_range_agrees_to_float_precision_only() {
        assert!(range_values_agree(&continuous(0.0, 10.0, 3.5), &continuous(0.0, 10.0, 3.5)));
        assert!(range_values_agree(&continuous(0.0, 10.0, 3.5), &continuous(0.0, 10.0, 3.5 + 1e-8)), "below 1e-6");
        assert!(!range_values_agree(&continuous(0.0, 10.0, 3.5), &continuous(0.0, 10.0, 3.5 + 1e-5)), "above 1e-6");
        assert!(!range_values_agree(&continuous(0.0, 10.0, 3.5), &continuous(0.0, 10.0, 3.6)));
    }

    #[test]
    fn values_that_have_no_number_never_agree() {
        let mut no_index = stepped(0.0, 10.0, 11, 0);
        no_index.as_object_mut().unwrap().remove("value_index");
        assert!(!range_values_agree(&no_index, &no_index.clone()));
        assert!(!range_values_agree(&json!("x"), &json!("x")));
    }
}
