cmake_minimum_required(VERSION 3.16)
project(rove_luau_vm LANGUAGES CXX)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

if(NOT DEFINED ROVE_LUAU_SOURCE)
    message(FATAL_ERROR "ROVE_LUAU_SOURCE must point to the pinned vendor/luau checkout")
endif()
# CONFIGURE_DEPENDS writes these paths into generated CMake code. Native
# Windows backslashes would become escapes such as \a in that generated file.
file(TO_CMAKE_PATH "${ROVE_LUAU_SOURCE}" ROVE_LUAU_SOURCE)

# MLua's Luau backend calls the public Lua C API with C linkage. The upstream
# sources are C++, so compile their public API declarations/definitions with
# C linkage while leaving all VM implementation internals as C++.
foreach(lib Common Ast Bytecode Compiler Config VM)
    file(GLOB sources CONFIGURE_DEPENDS "${ROVE_LUAU_SOURCE}/${lib}/src/*.cpp")
    if(lib STREQUAL "VM")
        list(APPEND sources "${CMAKE_CURRENT_SOURCE_DIR}/src/abi_probe.cpp")
    endif()
    add_library(Luau.${lib} STATIC ${sources})
    foreach(headers Common Ast Bytecode Compiler Config VM)
        target_include_directories(Luau.${lib} PUBLIC "${ROVE_LUAU_SOURCE}/${headers}/include")
    endforeach()
    target_compile_definitions(Luau.${lib} PRIVATE
        "LUA_API=extern \"C\""
        # mlua-sys hardcodes this value for pseudo-indices; matching it is
        # required for LUA_REGISTRYINDEX and the other pseudo-index constants.
        "LUAI_MAXCSTACK=1000000")
endforeach()

# MLua's require-by-string API is an official Luau library. The main checkout
# omits this standalone optional library, so build the source bundled below
# against the repository's independently pinned VM headers.
file(GLOB require_sources CONFIGURE_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/vendor/luau-Require/src/*.cpp")
add_library(Luau.Require STATIC ${require_sources})
target_include_directories(Luau.Require PUBLIC
    "${CMAKE_CURRENT_SOURCE_DIR}/vendor/luau-Require/include"
    "${ROVE_LUAU_SOURCE}/Common/include"
    "${ROVE_LUAU_SOURCE}/Ast/include"
    "${ROVE_LUAU_SOURCE}/Bytecode/include"
    "${ROVE_LUAU_SOURCE}/Compiler/include"
    "${ROVE_LUAU_SOURCE}/VM/include"
    "${ROVE_LUAU_SOURCE}/Config/include")
target_compile_definitions(Luau.Require PRIVATE
    "LUA_API=extern \"C\""
    "LUAI_MAXCSTACK=1000000")
target_compile_features(Luau.Require PRIVATE cxx_std_17)

target_compile_definitions(Luau.Compiler PRIVATE "LUACODE_API=extern \"C\"")

install(TARGETS Luau.Common Luau.Ast Luau.Bytecode Luau.Compiler Luau.Config Luau.VM Luau.Require
    ARCHIVE DESTINATION lib)
