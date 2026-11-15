//! Compact pose channels. The JSON timeline keeps markers, not a second pose tree.
use super::{err, Json};
use crate::rust_host::cframe::{self, CFrameValue};
use std::collections::BTreeMap;

pub(super) struct Clip {
    timeline: Json,
    pub channels: Vec<Channel>,
}
impl std::ops::Deref for Clip {
    type Target = Json;
    fn deref(&self) -> &Json {
        &self.timeline
    }
}
pub(super) struct Channel {
    pub path: Vec<String>,
    keys: Vec<Key>,
}
struct Key {
    time: f64,
    frame: CFrameValue,
    weight: f64,
    style: i32,
    direction: i32,
}

fn number(value: &Json, default: f64) -> mlua::Result<f64> {
    if value.is_null() {
        return Ok(default);
    }
    value
        .as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| err("invalid pose number"))
}
fn enumeration(value: &Json, default: i32, max: i32) -> mlua::Result<i32> {
    if value.is_null() {
        return Ok(default);
    }
    value
        .as_i64()
        .filter(|n| (0..=max as i64).contains(n))
        .map(|n| n as i32)
        .ok_or_else(|| err("invalid pose easing enum"))
}
fn array<const N: usize>(value: &Json) -> mlua::Result<[f64; N]> {
    let values = value
        .as_array()
        .filter(|a| a.len() == N)
        .ok_or_else(|| err("invalid pose frame shape"))?;
    let mut out = [0.; N];
    for (dst, src) in out.iter_mut().zip(values) {
        *dst = number(src, f64::NAN)?;
    }
    if out
        .iter()
        .any(|v| !v.is_finite() || v.abs() > f32::MAX as f64)
    {
        return Err(err("pose frame must be finite"));
    }
    Ok(out)
}
impl Clip {
    pub fn compile(input: &Json) -> mlua::Result<Self> {
        let mut channels: BTreeMap<Vec<String>, Vec<Key>> = BTreeMap::new();
        let mut count = 0usize;
        for frame in input["keyframes"]
            .as_array()
            .ok_or_else(|| err("invalid animation keyframes"))?
        {
            let time = frame["time"]
                .as_f64()
                .ok_or_else(|| err("invalid animation time"))?;
            // Older marker-only responses have no pose arrays.
            let Some(poses) = frame.get("poses") else {
                continue;
            };
            for pose in poses
                .as_array()
                .ok_or_else(|| err("invalid animation poses"))?
            {
                count += 1;
                if count > 65_536 {
                    return Err(err("animation pose sample limit exceeded"));
                }
                let path = pose["path"]
                    .as_array()
                    .filter(|p| !p.is_empty() && p.len() <= 64)
                    .ok_or_else(|| err("invalid pose path"))?
                    .iter()
                    .map(|name| {
                        name.as_str()
                            .filter(|s| s.len() <= 256)
                            .map(str::to_owned)
                            .ok_or_else(|| err("invalid pose name"))
                    })
                    .collect::<mlua::Result<Vec<_>>>()?;
                if !channels.contains_key(&path) && channels.len() >= 4096 {
                    return Err(err("animation channel limit exceeded"));
                }
                let position = array::<3>(&pose["position"])?;
                let rotation = array::<9>(&pose["rotation"])?;
                for i in 0..3 {
                    for j in 0..3 {
                        let dot = (0..3)
                            .map(|k| rotation[k * 3 + i] * rotation[k * 3 + j])
                            .sum::<f64>();
                        if (dot - if i == j { 1. } else { 0. }).abs() > 2e-4 {
                            return Err(err("pose rotation must be orthonormal"));
                        }
                    }
                }
                let r = rotation;
                if r[0] * (r[4] * r[8] - r[5] * r[7]) - r[1] * (r[3] * r[8] - r[5] * r[6])
                    + r[2] * (r[3] * r[7] - r[4] * r[6])
                    < 0.
                {
                    return Err(err("pose rotation must be right handed"));
                }
                let weight = number(&pose["weight"], 1.)?;
                if !(0. ..=1.).contains(&weight) {
                    return Err(err("pose weight must be in 0..1"));
                }
                let key = Key {
                    time,
                    frame: CFrameValue::new(position, rotation),
                    weight,
                    style: enumeration(&pose["easing_style"], 0, 5)?,
                    direction: enumeration(&pose["easing_direction"], 0, 2)?,
                };
                let keys = channels.entry(path).or_default();
                // Equal-time keys are ambiguous; fail the whole response before installing it.
                if keys.last().is_some_and(|k| k.time >= time) {
                    return Err(err("duplicate or unordered pose key"));
                }
                keys.push(key);
            }
        }
        // Keep timeline metadata without first cloning every numeric pose.
        let mut timeline = Json::Object(
            input
                .as_object()
                .ok_or_else(|| err("invalid animation clip"))?
                .iter()
                .filter(|(key, _)| key.as_str() != "keyframes")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );
        timeline["keyframes"] = Json::Array(
            input["keyframes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|frame| {
                    Json::Object(
                        frame
                            .as_object()
                            .unwrap()
                            .iter()
                            .filter(|(key, _)| key.as_str() != "poses")
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect(),
                    )
                })
                .collect(),
        );
        Ok(Self {
            timeline,
            channels: channels
                .into_iter()
                .map(|(path, keys)| Channel { path, keys })
                .collect(),
        })
    }
}

impl Channel {
    pub fn sample(&self, time: f64) -> mlua::Result<(CFrameValue, f64)> {
        let end = self.keys.partition_point(|k| k.time <= time);
        let a = &self.keys[end.saturating_sub(1)];
        let Some(b) = self.keys.get(end).filter(|_| end > 0) else {
            return Ok((a.frame.clone(), a.weight));
        };
        let t = ((time - a.time) / (b.time - a.time)).clamp(0., 1.);
        let eased = pose_ease(a.style, a.direction, t);
        Ok((
            CFrameValue::new(
                std::array::from_fn(|i| {
                    a.frame.position[i] + (b.frame.position[i] - a.frame.position[i]) * eased
                }),
                cframe::slerp(a.frame.rotation, b.frame.rotation, eased)?,
            ),
            (a.weight + (b.weight - a.weight) * t).clamp(0., 1.),
        ))
    }
}
fn pose_ease(style: i32, direction: i32, t: f64) -> f64 {
    if t <= 0. || t >= 1. {
        return t;
    }
    match style {
        0 => t,
        // Constant holds the earlier sample until the next key.
        1 => 0.,
        // Legacy Pose directions are reversed relative to TweenService.
        2 => super::ease(
            7,
            if direction < 2 {
                1 - direction
            } else {
                direction
            },
            t,
        ),
        3 => super::ease(
            10,
            if direction < 2 {
                1 - direction
            } else {
                direction
            },
            t,
        ),
        4 => super::ease(
            6,
            if direction < 2 {
                1 - direction
            } else {
                direction
            },
            t,
        ),
        _ => super::ease(10, direction, t),
    }
}

#[cfg(test)]
#[path = "animation_clip_tests.rs"]
mod tests;
