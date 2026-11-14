//! the rust-owned engine and host around the official luau vm.
//! rust-host is the default; the old host is an explicit comparison build.

pub use rove_runtime_sys::mlua as lua_api;

pub mod actor_io;
pub mod actor_shadow;
pub mod actors;
pub mod bindables;
pub mod cframe;
pub mod closures;
pub mod drawing;
pub mod enums;
pub mod errors;
pub mod geometry;
pub mod gui;
pub mod host_io_bindings;
pub mod idle;
pub mod input;
pub mod instance_cache;
pub mod instance_helpers;
pub mod integrity;
pub mod interactions;
pub mod lifecycle;
pub mod misc_values;
pub mod movement;
pub mod numeric;
pub mod playback;
pub(crate) mod program_context;
pub mod remotes;
pub mod scheduler;
pub mod sequences;
pub mod service_requests;
pub mod services;
pub mod session;
pub mod shared_tables;
pub mod signals;
pub mod source_helpers;
pub mod standalone;
pub mod temporal;
pub mod value_types;
pub mod values;
pub mod world;
pub(crate) mod world_aliases;

pub fn install_bindings(lua: &mlua::Lua, ctx: &world::WorldRef) -> mlua::Result<()> {
    errors::install(lua)?;
    integrity::install(lua)?;
    enums::install(lua)?;
    numeric::install(lua)?;
    cframe::install(lua)?;
    closures::install(lua)?;
    value_types::install(lua)?;
    sequences::install(lua)?;
    temporal::install(lua)?;
    signals::install(lua, ctx)?;
    idle::install(lua, ctx)?;
    world::install(lua, ctx)?;
    bindables::install(lua, ctx)?;
    misc_values::install(lua, ctx)?;
    geometry::install(lua, ctx)?;
    movement::install(lua, ctx)?;
    input::install(lua, ctx)?;
    gui::install(lua, ctx)?;
    drawing::install(lua, ctx)?;
    playback::install(lua, ctx)?;
    services::install(lua, ctx)?;
    service_requests::install(lua, ctx)?;
    source_helpers::install(lua, ctx)?;
    host_io_bindings::install(lua, ctx)?;
    remotes::install(lua, ctx)?;
    instance_helpers::install(lua, ctx)?;
    interactions::install(lua, ctx)?;
    shared_tables::install(lua, ctx, shared_tables::SharedTableRegistry::default())?;
    lifecycle::install(lua, ctx)?;
    Ok(())
}

#[cfg(test)]
mod bindables_tests;
#[cfg(test)]
mod closures_tests;
#[cfg(test)]
mod errors_tests;
#[cfg(test)]
mod host_io_tests;
#[cfg(test)]
mod host_tests;
#[cfg(test)]
mod instance_helpers_tests;
#[cfg(test)]
mod numeric_tests;
#[cfg(test)]
mod services_tests;
#[cfg(test)]
mod signals_tests;
#[cfg(test)]
mod world_tests;
