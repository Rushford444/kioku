.global _start
.text
_start:
    ldr x19, [sp]
    cmp x19, 2
    b.lt failure
    ldr x20, [sp, 16]
    mov x9, #6250
    mov x11, #64
    mul x9, x9, x11
outer:
    mov x10, #1000
inner:
    subs x10, x10, #1
    b.ne inner
    subs x9, x9, #1
    b.ne outer
    mov x0, x20
    bl read_file
    cbnz x0, failure
    mov x0, 0
    b done
failure:
    mov x0, 1
done:
    mov x8, 93
    svc 0
read_file:
    mov x1, x0
    mov x0, -100
    mov x2, 0
    mov x3, 0
    mov x8, 56
    svc 0
    cmp x0, 0
    b.lt read_error
    mov x21, x0
    sub sp, sp, 4096
read_loop:
    mov x0, x21
    mov x1, sp
    mov x2, 4096
    mov x8, 63
    svc 0
    cmp x0, 0
    b.gt read_loop
    b.lt read_error_stack
    add sp, sp, 4096
    mov x0, x21
    mov x8, 57
    svc 0
    cmp x0, 0
    cset x0, ne
    ret
read_error_stack:
    add sp, sp, 4096
read_error:
    mov x0, 1
    ret
