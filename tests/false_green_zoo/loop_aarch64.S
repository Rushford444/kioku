.global _start
.text
_start:
    mov x19, #5000
loop:
    mov x0, #-100
    adr x1, path
    mov x2, #0
    mov x3, #0
    mov x8, #56
    svc #0
    cmp x0, #0
    b.lt fail
    mov x8, #57
    svc #0
    subs x19, x19, #1
    b.ne loop
    mov x0, #0
    mov x8, #93
    svc #0
fail:
    mov x0, #1
    mov x8, #93
    svc #0
path:
    .asciz "input.txt"
