/* A stand-in for the agent's listening side, for the peer-check tests in CI
 * (Tests/FogarWireTests/SignedAgentTests.swift): binds a Unix socket at argv[1], and for each
 * connection writes one byte and waits for the peer to close. It is signed several ways by
 * scripts/ci-signed-agents.sh; what the test checks is the signature of this process. */
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 2 || strlen(argv[1]) >= sizeof(((struct sockaddr_un *)0)->sun_path)) {
        fprintf(stderr, "usage: peer-agent <socket path>\n");
        return 2;
    }
    signal(SIGPIPE, SIG_IGN);
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    if (s < 0) return 1;
    struct sockaddr_un a;
    memset(&a, 0, sizeof a);
    a.sun_family = AF_UNIX;
    strncpy(a.sun_path, argv[1], sizeof a.sun_path - 1);
    unlink(argv[1]);
    if (bind(s, (struct sockaddr *)&a, sizeof a) != 0 || listen(s, 4) != 0) return 1;
    for (;;) {
        int c = accept(s, NULL, NULL);
        if (c < 0) continue;
        char b = 'k';
        if (write(c, &b, 1) == 1) {
            while (read(c, &b, 1) > 0) {
            }
        }
        close(c);
    }
}
