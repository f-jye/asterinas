/* SCM_RIGHTS regression tests for Asterinas.
 *
 * T1: single-hop fd passing over a socketpair (fork, send, receive,
 *     verify via fstat).
 * T2: three-process relay: A -> B -> C, where B receives the fd and
 *     re-sends it, mimicking how dbus-daemon forwards TakeDevice fds.
 * T3: the GDBus read pattern: one sendmsg carrying a 16-byte "header"
 *     plus body and the fd in the cmsg; the receiver reads the header
 *     and the body in two separate recvmsg calls.
 * T4/T5: SO_PASSCRED (SCM_CREDENTIALS + SCM_RIGHTS in one recvmsg),
 *     the dbus-daemon receive configuration.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int failures = 0;

#define CHECK(cond, msg)                                                  \
        do {                                                              \
                if (cond)                                                 \
                        printf("PASS: %s\n", msg);                        \
                else {                                                    \
                        printf("FAIL: %s (errno=%d %s)\n", msg, errno,    \
                               strerror(errno));                          \
                        failures++;                                       \
                }                                                         \
        } while (0)

/* Sends `fd` together with `hdr_len + body_len` bytes of payload. */
static void send_with_fd(int sock, const char *payload, size_t len, int fd)
{
        char control[CMSG_SPACE(sizeof(int))];
        struct iovec iov = { (void *)payload, len };
        struct msghdr msg;
        struct cmsghdr *cmsg;

        memset(&msg, 0, sizeof(msg));
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control;
        msg.msg_controllen = sizeof(control);
        cmsg = CMSG_FIRSTHDR(&msg);
        cmsg->cmsg_level = SOL_SOCKET;
        cmsg->cmsg_type = SCM_RIGHTS;
        cmsg->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(cmsg), &fd, sizeof(int));
        msg.msg_controllen = cmsg->cmsg_len;

        if (sendmsg(sock, &msg, 0) < 0) {
                perror("sendmsg");
                exit(1);
        }
}

/* Receives a message and its fd; returns the fd (or -1) and the bytes. */
static int recv_with_fd(int sock, char *buf, size_t buf_len, ssize_t *out_len)
{
        char control[CMSG_SPACE(sizeof(int)) * 4];
        struct iovec iov = { buf, buf_len };
        struct msghdr msg;
        struct cmsghdr *cmsg;
        int fd = -1;
        ssize_t n;

        memset(&msg, 0, sizeof(msg));
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control;
        msg.msg_controllen = sizeof(control);

        n = recvmsg(sock, &msg, MSG_CMSG_CLOEXEC);
        if (n < 0) {
                perror("recvmsg");
                return -1;
        }
        *out_len = n;
        for (cmsg = CMSG_FIRSTHDR(&msg); cmsg; cmsg = CMSG_NXTHDR(&msg, cmsg)) {
                if (cmsg->cmsg_level == SOL_SOCKET &&
                    cmsg->cmsg_type == SCM_RIGHTS) {
                        memcpy(&fd, CMSG_DATA(cmsg), sizeof(int));
                        break;
                }
        }
        return fd;
}

static int is_char_dev_fd(int fd)
{
        struct stat st;
        if (fstat(fd, &st) < 0)
                return 0;
        return S_ISCHR(st.st_mode) || S_ISREG(st.st_mode);
}

/* T1: socketpair, parent sends /dev/null fd, child receives and checks. */
static void test_single_hop(void)
{
        int sv[2];
        pid_t pid;
        int status;

        CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "T1 socketpair");
        pid = fork();
        if (pid == 0) {
                char buf[64];
                ssize_t len;
                int fd = recv_with_fd(sv[1], buf, sizeof(buf), &len);
                if (fd < 0)
                        _exit(1);
                printf("T1 child: received fd=%d len=%zd\n", fd, len);
                if (!is_char_dev_fd(fd))
                        _exit(1);
                _exit(0);
        }
        int devnull = open("/dev/null", O_RDWR);
        char payload[116];
        memset(payload, 'A', sizeof(payload));
        send_with_fd(sv[0], payload, sizeof(payload), devnull);
        waitpid(pid, &status, 0);
        CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0,
              "T1 single-hop fd passing");
        close(devnull);
        close(sv[0]);
        close(sv[1]);
}

/* T2: A -> B -> C relay like dbus-daemon. Runs in one process with
 * socketpairs; the "relay" re-sends the fd it received. */
static void test_relay(void)
{
        int ab[2], bc[2];
        char buf[64];
        ssize_t len;
        int mid_fd, out_fd;

        CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, ab) == 0, "T2 socketpair ab");
        CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, bc) == 0, "T2 socketpair bc");

        int devnull = open("/dev/null", O_RDWR);
        char payload[116];
        memset(payload, 'B', sizeof(payload));

        /* A sends to B. */
        send_with_fd(ab[0], payload, sizeof(payload), devnull);
        /* B receives ... */
        mid_fd = recv_with_fd(ab[1], buf, sizeof(buf), &len);
        CHECK(mid_fd >= 0, "T2 relay received fd");
        /* ... and forwards the RECEIVED fd to C. */
        if (mid_fd >= 0)
                send_with_fd(bc[0], payload, sizeof(payload), mid_fd);
        /* C receives. */
        out_fd = recv_with_fd(bc[1], buf, sizeof(buf), &len);
        CHECK(out_fd >= 0 && is_char_dev_fd(out_fd),
              "T2 relayed fd usable at C");
        if (out_fd >= 0)
                close(out_fd);
        if (mid_fd >= 0)
                close(mid_fd);
        close(devnull);
        close(ab[0]);
        close(ab[1]);
        close(bc[0]);
        close(bc[1]);
}

/* T3: GDBus pattern: one sendmsg with header+body+fd, receiver reads the
 * 16-byte header and the body in two separate recvmsg calls. */
static void test_split_read(void)
{
        int sv[2];
        char payload[116];
        char buf[16];
        ssize_t len;
        int fd;

        CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "T3 socketpair");
        int devnull = open("/dev/null", O_RDWR);
        memset(payload, 'C', sizeof(payload));

        send_with_fd(sv[0], payload, sizeof(payload), devnull);

        /* First recvmsg: only the 16-byte header. */
        fd = recv_with_fd(sv[1], buf, sizeof(buf), &len);
        CHECK(len == 16, "T3 header read got 16 bytes");
        CHECK(fd >= 0, "T3 fd delivered with the first recvmsg");
        if (fd < 0) {
                close(devnull);
                close(sv[0]);
                close(sv[1]);
                return;
        }
        CHECK(is_char_dev_fd(fd), "T3 fd usable");

        /* Second recvmsg: the rest of the body, no fd expected. */
        char body[128];
        fd = recv_with_fd(sv[1], body, sizeof(body), &len);
        CHECK(len == 100, "T3 body read got the remaining bytes");
        CHECK(fd == -1, "T3 no spurious fd on the body read");

        close(devnull);
        close(sv[0]);
        close(sv[1]);
}

/* T4: the dbus-daemon pattern: the receiver enables SO_PASSCRED, so the
 * receive must carry BOTH an SCM_CREDENTIALS and an SCM_RIGHTS cmsg in
 * one recvmsg. */
static void test_passcred_combo(void)
{
        int sv[2];
        char buf[64];
        ssize_t len;
        int fd;
        int one = 1;

        CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "T4 socketpair");
        CHECK(setsockopt(sv[1], SOL_SOCKET, SO_PASSCRED, &one,
                         sizeof(one)) == 0, "T4 setsockopt SO_PASSCRED");

        int devnull = open("/dev/null", O_RDWR);
        char payload[116];
        memset(payload, 'D', sizeof(payload));
        send_with_fd(sv[0], payload, sizeof(payload), devnull);

        /* Receive with both cmsgs expected. */
        char control[256];
        struct iovec iov = { buf, sizeof(buf) };
        struct msghdr msg;
        struct cmsghdr *cmsg;
        int got_rights = 0, got_cred = 0, rcv_fd = -1;

        memset(&msg, 0, sizeof(msg));
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control;
        msg.msg_controllen = sizeof(control);
        ssize_t n = recvmsg(sv[1], &msg, MSG_CMSG_CLOEXEC);
        CHECK(n > 0, "T4 recvmsg returned data");
        for (cmsg = CMSG_FIRSTHDR(&msg); cmsg; cmsg = CMSG_NXTHDR(&msg, cmsg)) {
                if (cmsg->cmsg_level == SOL_SOCKET &&
                    cmsg->cmsg_type == SCM_RIGHTS) {
                        memcpy(&rcv_fd, CMSG_DATA(cmsg), sizeof(int));
                        got_rights = 1;
                } else if (cmsg->cmsg_level == SOL_SOCKET &&
                           cmsg->cmsg_type == SCM_CREDENTIALS) {
                        got_cred = 1;
                }
        }
        CHECK(got_cred, "T4 SCM_CREDENTIALS delivered");
        CHECK(got_rights, "T4 SCM_RIGHTS delivered alongside credentials");
        CHECK(rcv_fd >= 0 && is_char_dev_fd(rcv_fd),
              "T4 fd usable with SO_PASSCRED");
        if (rcv_fd >= 0)
                close(rcv_fd);
        close(devnull);
        close(sv[0]);
        close(sv[1]);
}

/* T5: like T4 but the fd is forwarded over a second hop after a
 * SO_PASSCRED receive, i.e. the full dbus-daemon relay. */
static void test_passcred_relay(void)
{
        int ab[2], bc[2];
        char buf[64];
        ssize_t len;
        int mid_fd, out_fd;
        int one = 1;
        char control[256];
        struct iovec iov = { buf, sizeof(buf) };
        struct msghdr msg;
        struct cmsghdr *cmsg;

        CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, ab) == 0, "T5 socketpair ab");
        CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, bc) == 0, "T5 socketpair bc");
        CHECK(setsockopt(ab[1], SOL_SOCKET, SO_PASSCRED, &one,
                         sizeof(one)) == 0, "T5 setsockopt SO_PASSCRED");

        int devnull = open("/dev/null", O_RDWR);
        char payload[116];
        memset(payload, 'E', sizeof(payload));
        send_with_fd(ab[0], payload, sizeof(payload), devnull);

        memset(&msg, 0, sizeof(msg));
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control;
        msg.msg_controllen = sizeof(control);
        ssize_t n = recvmsg(ab[1], &msg, MSG_CMSG_CLOEXEC);
        CHECK(n > 0, "T5 relay recvmsg");
        mid_fd = -1;
        for (cmsg = CMSG_FIRSTHDR(&msg); cmsg; cmsg = CMSG_NXTHDR(&msg, cmsg)) {
                if (cmsg->cmsg_level == SOL_SOCKET &&
                    cmsg->cmsg_type == SCM_RIGHTS) {
                        memcpy(&mid_fd, CMSG_DATA(cmsg), sizeof(int));
                }
        }
        CHECK(mid_fd >= 0, "T5 fd received under SO_PASSCRED");
        if (mid_fd < 0) {
                close(devnull);
                return;
        }
        /* Forward the received fd to C. */
        send_with_fd(bc[0], payload, sizeof(payload), mid_fd);
        out_fd = recv_with_fd(bc[1], buf, sizeof(buf), &len);
        CHECK(out_fd >= 0 && is_char_dev_fd(out_fd),
              "T5 forwarded fd usable at C");
        if (out_fd >= 0)
                close(out_fd);
        close(mid_fd);
        close(devnull);
        close(ab[0]);
        close(ab[1]);
        close(bc[0]);
        close(bc[1]);
}

/* T6: sd-bus/dbus-daemon send pattern: the message goes out as TWO iovecs
 * (16-byte header + body) with the fd in the cmsg. The receiver reads the
 * header first (16 bytes), then the body. */
static void send_two_iovec_with_fd(int sock, int fd)
{
        char header[16];
        char body[100];
        char control[CMSG_SPACE(sizeof(int))];
        struct iovec iov[2];
        struct msghdr msg;
        struct cmsghdr *cmsg;

        memset(header, 'H', sizeof(header));
        memset(body, 'B', sizeof(body));
        memset(&msg, 0, sizeof(msg));
        iov[0].iov_base = header;
        iov[0].iov_len = sizeof(header);
        iov[1].iov_base = body;
        iov[1].iov_len = sizeof(body);
        msg.msg_iov = iov;
        msg.msg_iovlen = 2;
        msg.msg_control = control;
        msg.msg_controllen = sizeof(control);
        cmsg = CMSG_FIRSTHDR(&msg);
        cmsg->cmsg_level = SOL_SOCKET;
        cmsg->cmsg_type = SCM_RIGHTS;
        cmsg->cmsg_len = CMSG_LEN(sizeof(int));
        memcpy(CMSG_DATA(cmsg), &fd, sizeof(int));
        msg.msg_controllen = cmsg->cmsg_len;

        if (sendmsg(sock, &msg, MSG_DONTWAIT | MSG_NOSIGNAL) < 0) {
                perror("T6 sendmsg");
                exit(1);
        }
}

static void test_two_iovec(void)
{
        int sv[2];
        char header[16];
        char body[128];
        ssize_t len;
        int fd;

        CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "T6 socketpair");
        int devnull = open("/dev/null", O_RDWR);

        send_two_iovec_with_fd(sv[0], devnull);

        /* GDBus-style read: header first, then body. */
        fd = recv_with_fd(sv[1], header, sizeof(header), &len);
        CHECK(len == 16, "T6 header read got 16 bytes");
        CHECK(fd >= 0, "T6 fd delivered with the header read");
        if (fd < 0) {
                close(devnull);
                close(sv[0]);
                close(sv[1]);
                return;
        }
        CHECK(is_char_dev_fd(fd), "T6 fd usable");
        fd = recv_with_fd(sv[1], body, sizeof(body), &len);
        CHECK(len == 100, "T6 body read got 100 bytes");
        CHECK(fd == -1, "T6 no spurious fd on the body read");

        close(devnull);
        close(sv[0]);
        close(sv[1]);
}

int main(void)
{
        test_single_hop();
        test_relay();
        test_split_read();
        test_passcred_combo();
        test_passcred_relay();
        test_two_iovec();
        printf(failures ? "FDTEST: %d FAILURES\n" : "FDTEST: ALL PASS\n",
               failures);
        return failures ? 1 : 0;
}
