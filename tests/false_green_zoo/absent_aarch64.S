.global _start
.text
_start:
    ldr x0, [sp]
    cmp x0, 2
    b.lt failure
    mov x0, -100
    ldr x1, [sp, 16]
    mov x2, 0
    mov x3, 0
    mov x8, 56
    svc 0
    cmn x0, 2
    b.ne failure
    mov x0, 0
    b done
failure:
    mov x0, 1
done:
    mov x8, 93
    svc 0
