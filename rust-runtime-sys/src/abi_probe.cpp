#include "lua.h"
#include "luacode.h"

#include <cstddef>

extern "C" int rove_luau_registry_index()
{
    return LUA_REGISTRYINDEX;
}

extern "C" int rove_luau_globals_index()
{
    return LUA_GLOBALSINDEX;
}

extern "C" int rove_luau_table_type()
{
    return LUA_TTABLE;
}

extern "C" int rove_luau_userdata_type()
{
    return LUA_TUSERDATA;
}

extern "C" int rove_luau_integer_type()
{
    return LUA_TINTEGER;
}

extern "C" int rove_luau_vector_type()
{
    return LUA_TVECTOR;
}

extern "C" size_t rove_luau_debug_size()
{
    return sizeof(lua_Debug);
}

extern "C" size_t rove_luau_callbacks_size()
{
    return sizeof(lua_Callbacks);
}

extern "C" size_t rove_luau_callbacks_userdata_offset()
{
    return offsetof(lua_Callbacks, userdata);
}

extern "C" size_t rove_luau_callbacks_interrupt_offset()
{
    return offsetof(lua_Callbacks, interrupt);
}

extern "C" size_t rove_luau_compile_options_size()
{
    return sizeof(lua_CompileOptions);
}
