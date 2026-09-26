/* A minimal GDBus TakeDevice client: calls TakeDevice(226,0) on the
 * login1 stub exactly the way mutter does, then validates the returned
 * fd. Pass BUSADDR=unix:path=... in the environment to target a private
 * bus (DBUS_SYSTEM_BUS_ADDRESS). */
#include <fcntl.h>
#include <gio/gio.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <unistd.h>

int main(int argc, char **argv)
{
        GError *error = NULL;
        GDBusConnection *conn;
        GVariant *result;
        GUnixFDList *fd_list = NULL;
        gint handle = -1, paused = -1;
        int fd, major = 226, minor = 0;

        if (argc >= 3) {
                major = atoi(argv[1]);
                minor = atoi(argv[2]);
        }

        conn = g_bus_get_sync(G_BUS_TYPE_SYSTEM, NULL, &error);
        if (!conn) {
                fprintf(stderr, "connect failed: %s\n", error->message);
                return 1;
        }

        result = g_dbus_connection_call_with_unix_fd_list_sync(
                conn,
                "org.freedesktop.login1",
                "/org/freedesktop/login1/session/c1",
                "org.freedesktop.login1.Session",
                "TakeDevice",
                g_variant_new("(uu)", major, minor),
                G_VARIANT_TYPE("(hb)"),
                G_DBUS_CALL_FLAGS_NONE,
                -1,
                NULL,           /* fd list in */
                &fd_list,       /* fd list out */
                NULL,
                &error);
        if (!result) {
                fprintf(stderr, "TakeDevice failed: %s\n", error->message);
                return 1;
        }

        g_variant_get(result, "(hb)", &handle, &paused);
        printf("reply: handle=%d paused=%d\n", handle, paused);
        if (!fd_list) {
                fprintf(stderr, "NO fd list in reply\n");
                return 1;
        }
        printf("fd list length: %d\n", g_unix_fd_list_get_length(fd_list));

        fd = g_unix_fd_list_get(fd_list, handle, &error);
        if (fd < 0) {
                fprintf(stderr, "get(%d) failed: %s\n", handle, error->message);
                return 1;
        }
        printf("got fd=%d\n", fd);

        struct stat st;
        if (fstat(fd, &st) < 0) {
                perror("fstat");
                return 1;
        }
        printf("fstat: dev=%lu mode=%o char=%d\n", (unsigned long) st.st_dev,
               st.st_mode, S_ISCHR(st.st_mode));

        if (major == 226) {
                /* Exercise it like mutter: drmGetCapabilities-ish ioctl. */
                struct {
                        unsigned long capability;
                        long value;
                } cap_req = { 0x1 /* DRM_CAP_DUMB_BUFFER */, 0 };
                if (ioctl(fd, 0xc0106400 /* DRM_IOCTL_GET_CAP */, &cap_req) < 0)
                        perror("DRM_GET_CAP");
                else
                        printf("DRM_CAP_DUMB_BUFFER value=%ld\n", cap_req.value);
        }

        close(fd);
        printf("TAKEDEVTEST: PASS\n");
        return 0;
}
