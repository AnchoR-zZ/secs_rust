# SECS_RUST

## Core Rules
1. **Struct field comments**: All fields in Rust structs and TypeScript interfaces/types must have clear comments explaining their purpose.
2. **Method comments**: All methods/functions must have clear comments describing their functionality, parameter meanings, and return values. Simple and self-explanatory methods can have brief comments, but none should be omitted; methods whose purpose is not immediately obvious must have detailed comments.
3. **Design before implementation**: When adding new features, architecture design must be proposed and confirmed with the user before any coding begins.
4. **File-level comments**: When creating a new code file, add a clear comment at the beginning of the file explaining the file’s purpose, its main responsibilities, and how it fits into the surrounding module or system. The comment should be concise but informative; avoid generic descriptions that simply repeat the filename.

   For module entry files such as Rust `lib.rs` or `mod.rs` that only declare or re-export modules, a brief module-level comment is sufficient. If the file contains only trivial module declarations and its purpose is obvious from the surrounding module structure, the comment may be minimal, but should still clarify the module’s role when helpful.
