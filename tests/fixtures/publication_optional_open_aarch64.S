.global _start
.text
_start:
    ldr x1, [sp, 16]
    mov x0, -100
    mov x2, 0
    mov x3, 0
    mov x8, 56
    svc 0
    cmp x0, 0
    b.lt done
    mov x8, 57
    svc 0
done:
    mov x0, 0
    mov x8, 93
    svc 0
