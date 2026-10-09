/* `compiler::asan` preprocesses this file: the token below shows that the compiler
   builds C with ASan. */
#if defined(__SANITIZE_ADDRESS__)
connector_opcua_asan
#elif defined(__has_feature)
#if __has_feature(address_sanitizer)
connector_opcua_asan
#endif
#endif
