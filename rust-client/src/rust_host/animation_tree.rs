//! Build an editable sequence one node at a time, sharing the compact clip.
use super::*;

pub(super) struct Builder {
    clip: Rc<animation_clip::Clip>,
    pub created: Vec<String>,
    pub root: Option<String>,
    rig: usize,
    frame_index: usize,
    frame: Option<String>,
    channel: usize,
    number: usize,
    marker: usize,
    paths: HashMap<Vec<String>, String>,
}

fn object(value: Json) -> serde_json::Map<String, Json> {
    value.as_object().unwrap().clone()
}
fn enum_value(kind: &str, value: i64) -> Json {
    json!({"$enum":{"type":kind,"value":value}})
}
fn finite(value: &Json, default: f64) -> mlua::Result<f64> {
    if value.is_null() {
        return Ok(default);
    }
    value
        .as_f64()
        .filter(|v| v.is_finite())
        .ok_or_else(|| err("invalid animation number"))
}
fn easing(value: &Json, max: i64) -> mlua::Result<i64> {
    if value.is_null() {
        return Ok(0);
    }
    value
        .as_i64()
        .filter(|v| (0..=max).contains(v))
        .ok_or_else(|| err("invalid pose easing"))
}
fn path(value: &Json) -> mlua::Result<Vec<String>> {
    value
        .as_array()
        .filter(|p| !p.is_empty() && p.len() <= 64)
        .ok_or_else(|| err("invalid number pose path"))?
        .iter()
        .map(|part| {
            part.as_str()
                .filter(|s| s.len() <= 256)
                .map(str::to_owned)
                .ok_or_else(|| err("invalid number pose name"))
        })
        .collect()
}
fn optional_array<'a>(value: &'a Json, key: &str) -> mlua::Result<&'a [Json]> {
    match value.get(key) {
        None => Ok(&[]),
        Some(Json::Array(rows)) => Ok(rows),
        _ => Err(err(format!("invalid animation {key}"))),
    }
}

impl Builder {
    pub fn new(clip: Rc<animation_clip::Clip>) -> mlua::Result<Self> {
        if !clip
            .get("name")
            .is_none_or(|v| v.as_str().is_some_and(|s| s.len() <= 1024))
        {
            return Err(err("invalid animation name"));
        }
        finite(&clip["authored_hip_height"], 1.35)?;
        // Validate auxiliary data before creating anything. Body poses were
        // validated while compiling the shared clip for playback.
        for rig in optional_array(&clip, "rig_data")? {
            rig_properties(rig)?;
        }
        let mut count = 0;
        for frame in clip["keyframes"].as_array().unwrap() {
            let mut seen = HashSet::new();
            for number in optional_array(frame, "number_poses")? {
                count += 1;
                let p = path(&number["path"])?;
                if count > 65_536
                    || p.len() < 3
                    || p[p.len() - 2] != "FaceControls"
                    || p[p.len() - 3] != "Head"
                    || !seen.insert(p)
                {
                    return Err(err("unsupported or duplicate number pose path"));
                }
                finite(&number["value"], 0.)?;
                let weight = finite(&number["weight"], 1.)?;
                if !(0. ..=1.).contains(&weight) {
                    return Err(err("invalid number pose weight"));
                }
                easing(&number["easing_style"], 5)?;
                easing(&number["easing_direction"], 2)?;
            }
        }
        Ok(Self {
            clip,
            created: Vec::new(),
            root: None,
            rig: 0,
            frame_index: 0,
            frame: None,
            channel: 0,
            number: 0,
            marker: 0,
            paths: HashMap::new(),
        })
    }

    fn create(
        &mut self,
        lua: &Lua,
        ctx: &WorldRef,
        class: &str,
        name: &str,
        parent: Option<&str>,
        properties: Json,
    ) -> mlua::Result<String> {
        if self.created.len() >= 65_536 {
            return Err(err("animation result node limit exceeded"));
        }
        let value =
            world::create_animation_node(lua, ctx, class, name, parent, object(properties))?;
        let id = world::instance_id(&value).ok_or_else(|| err("animation node creation failed"))?;
        self.created.push(id.clone());
        Ok(id)
    }

    /// Returns true when complete; otherwise creates at most one node.
    pub fn advance(&mut self, lua: &Lua, ctx: &WorldRef) -> mlua::Result<bool> {
        let clip = self.clip.clone();
        let Some(root) = self.root.clone() else {
            self.root = Some(self.create(lua, ctx, "KeyframeSequence",
                clip["name"].as_str().unwrap_or("KeyframeSequence"), None,
                json!({"Loop":clip["looped"],"Priority":enum_value("AnimationPriority",clip["priority"].as_i64().unwrap_or(1000)),
                "AuthoredHipHeight":finite(&clip["authored_hip_height"],1.35)?}))?);
            return Ok(false);
        };
        if !ctx.borrow().nodes.get(&root).is_some_and(|n| !n.destroyed) {
            return Err(err("animation result was destroyed during loading"));
        }
        let rigs = optional_array(&clip, "rig_data")?;
        if let Some(rig) = rigs.get(self.rig) {
            self.create(
                lua,
                ctx,
                "AnimationRigData",
                rig["name"].as_str().unwrap_or("AnimationRigData"),
                Some(&root),
                Json::Object(rig_properties(rig)?),
            )?;
            self.rig += 1;
            return Ok(false);
        }
        loop {
            let Some(frame) = clip["keyframes"].as_array().unwrap().get(self.frame_index) else {
                return Ok(true);
            };
            let Some(frame_id) = self.frame.clone() else {
                self.frame = Some(self.create(
                    lua,
                    ctx,
                    "Keyframe",
                    frame["name"].as_str().unwrap(),
                    Some(&root),
                    json!({"Time":frame["time"]}),
                )?);
                return Ok(false);
            };
            while let Some(channel) = clip.channels.get(self.channel) {
                let Some(key) = channel.key_at_frame(self.frame_index) else {
                    self.channel += 1;
                    continue;
                };
                // Sparse manually supplied channels may omit structural parents.
                // Their weight is zero so rebuilding does not add active motion.
                for depth in 1..channel.path.len() {
                    if !self.paths.contains_key(&channel.path[..depth]) {
                        let parent = if depth == 1 {
                            frame_id.clone()
                        } else {
                            self.paths[&channel.path[..depth - 1]].clone()
                        };
                        let id = self.create(
                            lua,
                            ctx,
                            "Pose",
                            &channel.path[depth - 1],
                            Some(&parent),
                            json!({"Weight":0.}),
                        )?;
                        self.paths.insert(channel.path[..depth].to_vec(), id);
                        return Ok(false);
                    }
                }
                let parent = if channel.path.len() == 1 {
                    frame_id.clone()
                } else {
                    self.paths[&channel.path[..channel.path.len() - 1]].clone()
                };
                let id = self.create(lua, ctx, "Pose", channel.path.last().unwrap(), Some(&parent),
                    json!({"CFrame":{"$cframe":{"position":key.frame.position,"rotation":key.frame.rotation}},
                    "Weight":key.weight,"MaskWeight":key.mask_weight,"EasingStyle":enum_value("PoseEasingStyle", key.style as i64),
                    "EasingDirection":enum_value("PoseEasingDirection", key.direction as i64)}))?;
                self.paths.insert(channel.path.clone(), id);
                self.channel += 1;
                return Ok(false);
            }
            if let Some(number) = optional_array(frame, "number_poses")?.get(self.number) {
                let path = path(&number["path"])?;
                let folder_path = &path[..path.len() - 1];
                let Some(folder) = self.paths.get(folder_path).cloned() else {
                    let parent = self
                        .paths
                        .get(&path[..path.len() - 2])
                        .cloned()
                        .ok_or_else(|| err("number pose has no body pose parent"))?;
                    let id =
                        self.create(lua, ctx, "Folder", "FaceControls", Some(&parent), json!({}))?;
                    self.paths.insert(folder_path.to_vec(), id);
                    return Ok(false);
                };
                self.create(lua, ctx, "NumberPose", path.last().unwrap(), Some(&folder),
                    json!({"Value":finite(&number["value"],0.)?,"Weight":finite(&number["weight"],1.)?,
                    "EasingStyle":enum_value("PoseEasingStyle",easing(&number["easing_style"],5)?),
                    "EasingDirection":enum_value("PoseEasingDirection",easing(&number["easing_direction"],2)?)}))?;
                self.number += 1;
                return Ok(false);
            }
            if let Some(marker) = frame["markers"].as_array().unwrap().get(self.marker) {
                self.create(
                    lua,
                    ctx,
                    "KeyframeMarker",
                    marker["name"].as_str().unwrap(),
                    Some(&frame_id),
                    json!({"Value":marker["value"]}),
                )?;
                self.marker += 1;
                return Ok(false);
            }
            self.frame_index += 1;
            self.frame = None;
            self.channel = 0;
            self.number = 0;
            self.marker = 0;
            self.paths.clear();
        }
    }

    pub fn cleanup_one(&mut self, lua: &Lua, ctx: &WorldRef) -> mlua::Result<bool> {
        // Children were created after their parents, so reverse order avoids
        // a giant recursive Destroy response when a long request is cancelled.
        if let Some(id) = self.created.pop() {
            world::destroy_instance(lua, ctx, &id)?;
        }
        Ok(self.created.is_empty())
    }
}

fn rig_properties(rig: &Json) -> mlua::Result<serde_json::Map<String, Json>> {
    if !rig
        .get("name")
        .is_none_or(|v| v.as_str().is_some_and(|s| s.len() <= 1024))
    {
        return Err(err("invalid animation rig name"));
    }
    let fields = rig["fields"]
        .as_object()
        .ok_or_else(|| err("invalid animation rig data"))?;
    let mut result = serde_json::Map::new();
    for (key, value) in fields {
        if !matches!(
            key.as_str(),
            "label" | "name" | "parent" | "preTransform" | "postTransform" | "transform"
        ) {
            return Err(err("unsupported animation rig field"));
        }
        let hex = value["bytes_hex"]
            .as_str()
            .filter(|s| {
                s.len() <= 64 * 1024 && s.len() % 2 == 0 && s.bytes().all(|b| b.is_ascii_hexdigit())
            })
            .ok_or_else(|| err("invalid animation rig bytes"))?;
        result.insert(key.clone(), json!({"$bytes_hex":hex}));
    }
    if serde_json::to_vec(&result)
        .map_err(mlua::Error::external)?
        .len()
        > 64 * 1024
    {
        return Err(err("animation rig data exceeds the node limit"));
    }
    Ok(result)
}
