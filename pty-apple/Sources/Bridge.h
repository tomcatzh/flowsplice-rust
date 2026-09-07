#include <stdint.h>
char *flowsplice_pty_open(const char *options);
char *flowsplice_pty_send(uint64_t handle, const char *action);
char *flowsplice_pty_poll(uint64_t handle);
char *flowsplice_pty_close(uint64_t handle);
void flowsplice_pty_string_free(char *value);
