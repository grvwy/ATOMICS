// A native executable gives the VM window its own Launch Services identity. Its sibling main app
// supplies the real binary, so repairing or updating ATOMICS never leaves a second stale binary.
#include <mach-o/dyld.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
    char executable[PATH_MAX], resolved[PATH_MAX], binary[PATH_MAX];
    uint32_t size = sizeof(executable);
    if (_NSGetExecutablePath(executable, &size) != 0 || realpath(executable, resolved) == NULL) {
        perror("ATOMICS VM: cannot locate the launcher");
        return 1;
    }
    for (int i = 0; i < 4; ++i) {
        char *slash = strrchr(resolved, '/');
        if (slash == NULL) return 1;
        *slash = '\0';
    }
    if (snprintf(binary, sizeof(binary), "%s/ATOMICS.app/Contents/MacOS/atomics", resolved) >= (int)sizeof(binary)) return 1;
    char **args = calloc((size_t)argc + 3, sizeof(char *));
    if (args == NULL) return 1;
    args[0] = binary;
    args[1] = "--connect";
    args[2] = "vm";
    int n = 3;
    for (int i = 1; i < argc; ++i) {
        if (strncmp(argv[i], "-psn_", 5) != 0) args[n++] = argv[i];
    }
    execv(binary, args);
    perror("ATOMICS VM: install ATOMICS.app beside this launcher");
    free(args);
    return 1;
}
