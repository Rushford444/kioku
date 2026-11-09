.global _start
.text
_start:
    mov (%rsp), %r12
    cmp $2, %r12
    jl fail
    mov 16(%rsp), %rdi
    call read_file
    test %rax, %rax
    jne fail
    xor %edi, %edi
    mov $60, %eax
    syscall
fail:
    mov $1, %edi
    mov $60, %eax
    syscall
read_file:
    mov $2, %eax
    xor %esi, %esi
    xor %edx, %edx
    syscall
    cmp $0, %rax
    jl err
    mov %rax, %r14
    sub $4096, %rsp
loop:
    mov %r14, %rdi
    mov %rsp, %rsi
    mov $4096, %edx
    xor %eax, %eax
    syscall
    cmp $0, %rax
    jg loop
    jl err_stack
    add $4096, %rsp
    mov %r14, %rdi
    mov $3, %eax
    syscall
    xor %eax, %eax
    ret
err_stack:
    add $4096, %rsp
err:
    mov $1, %eax
    ret
