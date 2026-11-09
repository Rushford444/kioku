.global _start
.text
_start:
    ldr x19, [sp]
    cmp x19, 2
    b.lt failure
    ldr x20, [sp, 16]
    mov x0, x20
    bl read_file
    cbnz x0, failure
    cmp x19, 3
    b.lt success
    mov x0, 17
    mov x1, 0
    mov x2, 0
    mov x3, 0
    mov x4, 0
    mov x8, 220
    svc 0
    cmp x0, 0
    b.lt failure
    b.eq child
    sub sp, sp, 16
    mov x1, sp
    mov x2, 0
    mov x3, 0
    mov x8, 260
    svc 0
    cmp x0, 0
    b.lt failure
    ldr w0, [sp]
    cbnz w0, failure
success:
    mov x0, 0
    b done
child:
    ldr x0, [sp, 24]
    bl read_file
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
