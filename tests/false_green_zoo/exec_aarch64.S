.global _start
.text
_start:
    ldr x0, [sp]
    cmp x0, 3
    b.lt failure
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
    mov x0, 0
    b done
child:
    ldr x0, [sp, 16]
    ldr x3, [sp, 24]
    sub sp, sp, 32
    stp x0, x3, [sp]
    str xzr, [sp, 16]
    mov x1, sp
    mov x2, 0
    mov x8, 221
    svc 0
failure:
    mov x0, 1
done:
    mov x8, 93
    svc 0
