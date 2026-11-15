//! Narrow local instance context for verified bytecode programs.
//!
//! This is intentionally a tiny fixture: one DataModel, one enabled
//! RunService, and the two legacy settings globals the native engine fixture
//! installs. It does not install the broader runtime service surface.
use mlua::{
    AnyUserData, Lua, MetaMethod, Result, UserData, UserDataFields, UserDataMethods, Value,
};
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum LocalKind {
    DataModel,
    RunService,
    UserSettings,
    GlobalSettings,
}

struct Context {
    job_id: Arc<str>,
    instances: RefCell<HashMap<LocalKind, mlua::RegistryKey>>,
}

#[derive(Clone)]
struct LocalInstance {
    context: Rc<Context>,
    kind: LocalKind,
}

impl LocalInstance {
    fn name(&self) -> &'static str {
        match self.kind {
            LocalKind::DataModel => "Game",
            LocalKind::RunService => "RunService",
            LocalKind::UserSettings => "GlobalBasicSettings",
            LocalKind::GlobalSettings => "GlobalSettings",
        }
    }
    fn class_name(&self) -> &'static str {
        match self.kind {
            LocalKind::DataModel => "DataModel",
            LocalKind::RunService => "RunService",
            LocalKind::UserSettings => "UserSettings",
            LocalKind::GlobalSettings => "GlobalSettings",
        }
    }
    fn child(&self, name: &str) -> Option<LocalKind> {
        if self.kind == LocalKind::DataModel && name == "RunService" {
            Some(LocalKind::RunService)
        } else {
            None
        }
    }
    fn same_context(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.context, &other.context)
    }
}

fn instance(lua: &Lua, context: &Rc<Context>, kind: LocalKind) -> Result<AnyUserData> {
    if let Some(key) = context.instances.borrow().get(&kind) {
        return lua.registry_value(key);
    }
    let userdata = lua.create_userdata(LocalInstance {
        context: context.clone(),
        kind,
    })?;
    let key = lua.create_registry_value(userdata.clone())?;
    context.instances.borrow_mut().insert(kind, key);
    Ok(userdata)
}
fn runtime_error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

impl UserData for LocalInstance {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "Instance");
        fields.add_field_method_get("Name", |_, this| Ok(this.name()));
        fields.add_field_method_get("ClassName", |_, this| Ok(this.class_name()));
        fields.add_field_method_get("JobId", |_, this| {
            Ok(
                if this.kind == LocalKind::DataModel && !this.context.job_id.is_empty() {
                    Some(this.context.job_id.to_string())
                } else {
                    None
                },
            )
        });
        fields.add_field_method_get("Parent", |lua, this| match this.kind {
            LocalKind::RunService => Ok(Value::UserData(instance(
                lua,
                &this.context,
                LocalKind::DataModel,
            )?)),
            _ => Ok(Value::Nil),
        });
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, (_key, _value): (Value, Value)| {
                Err::<(), _>(runtime_error("local Instance properties are read-only"))
            },
        );
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| Ok(this.name()));
        methods.add_meta_method(MetaMethod::Eq, |_, this, other: AnyUserData| {
            Ok(other
                .borrow::<Self>()
                .map(|other| this.same_context(&other) && this.kind == other.kind)
                .unwrap_or(false))
        });

        methods.add_method("IsA", |_, this, class_name: String| {
            let class = this.class_name();
            let result = class == class_name
                || class_name == "Instance"
                || class_name == "Object"
                || (this.kind == LocalKind::DataModel && class_name == "ServiceProvider")
                || (matches!(
                    this.kind,
                    LocalKind::UserSettings | LocalKind::GlobalSettings
                ) && matches!(class_name.as_str(), "GenericSettings" | "ServiceProvider"));
            Ok(result)
        });
        methods.add_method("GetService", |lua, this, service_name: String| {
            if this.kind != LocalKind::DataModel {
                return Err(runtime_error(
                    "GetService requires the local DataModel fixture",
                ));
            }
            match this.child(&service_name) {
                Some(kind) => Ok(Value::UserData(instance(lua, &this.context, kind)?)),
                None => Err(runtime_error(format!(
                    "service '{service_name}' is not enabled in this local fixture"
                ))),
            }
        });
        methods.add_method("FindService", |lua, this, service_name: String| {
            if this.kind != LocalKind::DataModel {
                return Err(runtime_error(
                    "FindService requires the local DataModel fixture",
                ));
            }
            match this.child(&service_name) {
                Some(kind) => Ok(Value::UserData(instance(lua, &this.context, kind)?)),
                None => Ok(Value::Nil),
            }
        });
        methods.add_method("FindFirstChild", |lua, this, child_name: String| {
            if this.kind == LocalKind::DataModel {
                if let Some(kind) = this.child(&child_name) {
                    return Ok(Value::UserData(instance(lua, &this.context, kind)?));
                }
            }
            Ok(Value::Nil)
        });
        methods.add_method("GetChildren", |lua, this, ()| {
            let children = lua.create_table()?;
            if let Some(kind) = this.child("RunService") {
                children.raw_set(1, instance(lua, &this.context, kind)?)?;
            }
            Ok(children)
        });
        methods.add_method("IsStudio", |_, this, ()| {
            if this.kind != LocalKind::RunService {
                return Err(runtime_error(
                    "IsStudio is only available on the local RunService fixture",
                ));
            }
            Ok(false)
        });
        methods.add_method("IsRunning", |_, this, ()| {
            if this.kind != LocalKind::RunService {
                return Err(runtime_error(
                    "IsRunning is only available on the local RunService fixture",
                ));
            }
            Ok(true)
        });
        methods.add_method("GetFFlag", |_, this, (_key,): (String,)| {
            if this.kind != LocalKind::GlobalSettings {
                return Err(runtime_error(
                    "GlobalSettings:GetFFlag requires GlobalSettings",
                ));
            }
            Err::<(), _>(runtime_error(
                "engine flag values are unavailable in this local runtime",
            ))
        });
        methods.add_method("GetFVariable", |_, this, (_key,): (String,)| {
            if this.kind != LocalKind::GlobalSettings {
                return Err(runtime_error(
                    "GlobalSettings:GetFVariable requires GlobalSettings",
                ));
            }
            Err::<(), _>(runtime_error(
                "engine variable values are unavailable in this local runtime",
            ))
        });
    }
}

/// Installs the restricted context for one verified bytecode job.
pub fn install(lua: &Lua, job_id: &str) -> Result<()> {
    let context = Rc::new(Context {
        job_id: Arc::from(job_id),
        instances: RefCell::new(HashMap::new()),
    });
    let game = instance(lua, &context, LocalKind::DataModel)?;
    lua.globals().set("game", game)?;
    let user_settings_context = context.clone();
    lua.globals().set(
        "UserSettings",
        lua.create_function(move |lua, args: mlua::Variadic<Value>| {
            if !args.is_empty() {
                return Err(runtime_error("UserSettings takes no arguments"));
            }
            Ok(instance(
                lua,
                &user_settings_context,
                LocalKind::UserSettings,
            )?)
        })?,
    )?;
    let settings_context = context.clone();
    lua.globals().set(
        "settings",
        lua.create_function(move |lua, args: mlua::Variadic<Value>| {
            if !args.is_empty() {
                return Err(runtime_error("settings takes no arguments"));
            }
            Ok(instance(lua, &settings_context, LocalKind::GlobalSettings)?)
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "program_context_tests.rs"]
mod tests;
