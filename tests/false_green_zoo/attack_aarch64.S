.global _start
.text
_start:
    ldr x0, [sp]
    cmp x0, 2
    b.lt finish
    ldr x0, [sp, 16]
    ldrb w0, [x0]
    cmp w0, 'n'
    b.eq network
    cmp w0, 'c'
    b.eq clock
    cmp w0, 'r'
    b.eq random
    cmp w0, 'i'
    b.eq input
    cmp w0, 'p'
    b.eq proc
    cmp w0, 'u'
    b.eq unknown
    cmp w0, 'a'
    b.eq uring
    cmp w0, 'w'
    b.eq write_alias
    cmp w0, 'm'
    b.eq shared_mapping
    cmp w0, 'f'
    b.eq inherited_fd
    b finish
network:
    mov x0, 2
    mov x1, 1
    mov x2, 0
    mov x8, 198
    svc 0
    b finish
clock:
    mov x0, 0
    adr x1, buffer
    mov x8, 113
    svc 0
    b finish
random:
    adr x0, buffer
    mov x1, 16
    mov x2, 0
    mov x8, 278
    svc 0
    b finish
input:
    mov x0, 0
    adr x1, buffer
    mov x2, 16
    mov x8, 63
    svc 0
    b finish
proc:
    mov x0, -100
    adr x1, path
    mov x2, 0
    mov x3, 0
    mov x8, 56
    svc 0
    b finish
unknown:
    mov x8, 999
    svc 0
    b finish
write_alias:
    mov x0, -100
    ldr x1, [sp, 24]
    mov x2, 2
    mov x3, 0
    mov x8, 56
    svc 0
    mov x19, x0
    adr x1, buffer
    mov x2, 1
    mov x8, 63
    svc 0
    cmp x0, 1
    b.ne failure
    mov x0, x19
    mov x1, 0
    mov x2, 0
    mov x8, 62
    svc 0
    cmp x0, 0
    b.ne failure
    mov w0, 'X'
    adr x1, buffer
    strb w0, [x1]
    mov x0, x19
    mov x2, 1
    mov x8, 64
    svc 0
    cmp x0, 1
    b.ne failure
    b finish
failure:
    mov x0, 1
    mov x8, 93
    svc 0
shared_mapping:
    mov x0, -100
    ldr x1, [sp, 24]
    mov x2, 2
    mov x3, 0
    mov x8, 56
    svc 0
    mov x4, x0
    mov x0, 0
    mov x1, 4096
    mov x2, 3
    mov x3, 1
    mov x5, 0
    mov x8, 222
    svc 0
    b finish
inherited_fd:
    mov x0, 3
    adr x1, buffer
    mov x2, 16
    mov x8, 63
    svc 0
    cmp x0, 0
    b.gt finish
    mov x0, 1
    mov x8, 93
    svc 0
uring:
    mov x0, 1
    adr x1, buffer
    mov x8, 425
    svc 0
finish:
    mov x0, 1
    adr x1, marker
    mov x2, 4
    mov x8, 64
    svc 0
    mov x0, 0
    mov x8, 93
    svc 0
.data
path: .asciz "/proc/self/status"
marker: .ascii "ran\n"
.balign 16
buffer: .space 256
