cmake_minimum_required(VERSION 3.22)
project(crossflow_pairing_spike LANGUAGES C CXX)

# Standalone dependency experiment; not linked into the Crossflow application.
# Supply the verified archive from source-lock.json; never download floating HEAD.
set(BORINGSSL_SOURCE_DIR "" CACHE PATH "Verified pinned BoringSSL source directory")
if(NOT EXISTS "${BORINGSSL_SOURCE_DIR}/include/openssl/curve25519.h")
    message(FATAL_ERROR "Set BORINGSSL_SOURCE_DIR to the verified pinned source")
endif()
set(BUILD_TESTING OFF CACHE BOOL "Upstream full test suite not run by this spike" FORCE)
set(OPENSSL_NO_ASM ON CACHE BOOL "Functional no-assembly build" FORCE)
add_subdirectory("${BORINGSSL_SOURCE_DIR}" boringssl EXCLUDE_FROM_ALL)
add_executable(spake2_test spake2_test.cc)
target_compile_features(spake2_test PRIVATE cxx_std_17)
target_link_libraries(spake2_test PRIVATE crypto)
