# Trace zoo inventory — not test results

The original plan listed 32 scenario rows. The current
[CLI zoo](../false_green_zoo.rs) defines 40 test functions: four Unix default
execution tests and 36 Linux/aarch64 traced tests. These counts are not
interchangeable: several tests share a planned scenario, and a parser test
is not a real-CLI attack. This inventory describes test code, not a test run
or a claim that every original acceptance condition is met.

The traced tests require Linux/aarch64, the pinned tracer, ptrace and a C
compiler for static helpers. Default tests do not establish traced reuse.
Names below are under `traced_readers` unless marked default. Related parser
coverage is in [trace tests](../trace.rs).

| Plan row | Related test definition(s) | Scope / limitation |
|---|---|---|
| self-text | source_bytes; source_text_change_cannot_reuse_a_default_result (default) | positive traced hit, AST requested/effective bytes, whitespace change; default execution separate |
| ignored | ignored_file; default_executes_twice_and_reflects_ignored_input_change (default) | traced input mutation; default separate |
| external | external_file | external file mutation |
| dotenv | dotenv_file | synthetic fixture content |
| symlink-content | symlink_target_content | target content mutation |
| symlink-retarget | symlink_retarget; symlink_retarget_must_never_hit; default_observes_symlink_retargeting (default) | traced positive hit then retarget; default separate |
| absent | previously_absent_path | create absent path |
| absent-lock | previously_absent_lockfile; default_observes_created_previously_absent_path (default) | lockfile creation |
| executable | changed_executable_image | appends byte to executable |
| directory-membership | directory_enumeration | new directory member |
| metadata | file_metadata; stat_metadata_query | chmod; distinct read and stat coverage |
| child-read | descendant_file | parent and child inputs and clone oracle |
| cwd | relative_open_after_chdir | chdir + relative open |
| dirfd | relative_open_via_dirfd | openat via directory descriptor |
| deletion | file_deleted_after_hit | deletes input after hit |
| private-mmap | private_file_mapping | read-only private mapping |
| crlf | line_endings | LF to CRLF |
| case-sensitive | case_sensitive_path | case-sensitive FS needed; not a cross-platform claim |
| full-env | full_environment_change | change undeclared variable; helper need not read it |
| child-exec | changed_child_executable | child image mutation |
| network | deny_network | real helper syscall and child status |
| clock | deny_clock | real helper syscall and child status |
| random | deny_random | real helper syscall and child status |
| stdin | deny_stdin | supplied stdin |
| inherited-fd | deny_inherited_fd_read | extra fd 3 |
| rewrite | deny_read_write_alias | verifies changed bytes |
| shared-mmap | deny_shared_writable_mapping | writable mapping denied |
| submodule | submodule_tracked_content | tracks ordinary file inside real submodule checkout, NOT gitlink metadata or unsupported object semantics |
| io-uring | deny_io_uring | oracle accepts success or EPERM, not separately proved success + EPERM + ENOSYS; denied trace reason unknown |
| device | deny_proc; deny_proc_through_symlink | /proc, not a general device matrix |
| incomplete-tree | tests/trace.rs::rejects_unknown_incomplete_and_forged_exit and parser_requires_initial_exec_and_root_completion | parser-level coverage, not this plan's real disqualifying child syscall + oracle requirement |
| unknown-syscall | deny_unknown_syscall | actual helper syscall |

The common traced dependency helper checks an eligible hit, a changed input,
and manifest record sensitivity; individual tests and independent trace checks
have different scopes. Denial tests use a supported positive control. Consult
the test code and actual gate logs for outcomes and prerequisites. No result
here implies macOS or Windows runtime verification.
