#include <stdio.h>
#include <stdlib.h>

int main(int argc, char *argv[], char *envp[]) {
    printf("envtest: argc=%d\n", argc);
    for (int i = 0; i < argc; i++) {
        printf("envtest: argv[%d]=%s\n", i, argv[i]);
    }

    printf("envtest: envp entries:\n");
    for (int i = 0; envp[i]; i++) {
        printf("envtest: envp[%d]=%s\n", i, envp[i]);
    }

    const char *path = getenv("PATH");
    const char *user = getenv("USER");
    const char *missing = getenv("DOES_NOT_EXIST");

    printf("envtest: getenv(PATH)=%s\n", path ? path : "(null)");
    printf("envtest: getenv(USER)=%s\n", user ? user : "(null)");
    printf("envtest: getenv(DOES_NOT_EXIST)=%s\n", missing ? missing : "(null)");

    if (setenv("FOO", "bar", 1) == 0) {
        printf("envtest: getenv(FOO) after setenv=%s\n", getenv("FOO"));
    } else {
        printf("envtest: setenv(FOO) FAILED\n");
    }

    unsetenv("FOO");
    printf("envtest: getenv(FOO) after unsetenv=%s\n", getenv("FOO") ? getenv("FOO") : "(null)");

    return 0;
}
