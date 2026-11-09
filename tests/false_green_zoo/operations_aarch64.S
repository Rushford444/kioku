.global _start
.text
_start:
    ldr x19, [sp, 16]
    ldrb w19, [x19]
    cmp w19, 'c'
    b.eq change_dir
    cmp w19, 'd'
    b.eq dir_fd
    cmp w19, 's'
    b.eq metadata
    cmp w19, 'l'
    b.eq listing
    cmp w19, 'm'
    b.eq mapping
    b fail
change_dir:
    ldr x0, [sp, 24]
    mov x8, 49
    svc 0
    cmp x0, 0
    b.lt fail
    mov x0, -100
    ldr x1, [sp, 32]
    b open_file
dir_fd:
    mov x0, -100
    ldr x1, [sp, 24]
    mov x2, 16384
    mov x3, 0
    mov x8, 56
    svc 0
    cmp x0, 0
    b.lt fail
    ldr x1, [sp, 32]
open_file:
    mov x2, 0
    mov x3, 0
    mov x8, 56
    svc 0
    cmp x0, 0
    b.lt fail
    adr x1, buffer
    mov x2, 16
    mov x8, 63
    svc 0
    cmp x0, 0
    b.lt fail
    b success
metadata:
    mov x0, -100
    ldr x1, [sp, 24]
    adr x2, buffer
    mov x3, 0
    mov x8, 79
    svc 0
    cmp x0, 0
    b.lt fail
    b success
listing:
    mov x0, -100
    ldr x1, [sp, 24]
    mov x2, 16384
    mov x3, 0
    mov x8, 56
    svc 0
    cmp x0, 0
    b.lt fail
    adr x1, buffer
    mov x2, 4096
    mov x8, 61
    svc 0
    cmp x0, 0
    b.lt fail
    b success
mapping:
    mov x0, -100
    ldr x1, [sp, 24]
    mov x2, 0
    mov x3, 0
    mov x8, 56
    svc 0
    cmp x0, 0
    b.lt fail
    mov x4, x0
    mov x0, 0
    mov x1, 4096
    mov x2, 1
    mov x3, 2
    mov x5, 0
    mov x8, 222
    svc 0
    cmp x0, 0
    b.lt fail
    ldrb w0, [x0]
success:
    mov x0, 0
    b done
fail:
    mov x0, 1
done:
    mov x8, 93
    svc 0
.bss
.balign 16
buffer: .space 4096
