//! The initial process stack (argc, argv, envp, auxv) `ld.so` boots from.
//!
//! It is leaked on purpose: the foreign libc keeps pointers into it
//! (`environ`, `__libc_stack_end`, `argv`).
