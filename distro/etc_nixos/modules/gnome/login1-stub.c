/* Minimal org.freedesktop.login1 stub: owns the bus name and exports the
 * Manager/Seat/Session objects with the properties and device methods that
 * mutter's native backend uses.  Device fds are opened directly. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <sd-bus.h>

#define BUS_NAME "org.freedesktop.login1"
#define MANAGER_PATH "/org/freedesktop/login1"
#define SEAT_PATH "/org/freedesktop/login1/seat/seat0"
#define SESSION_PATH "/org/freedesktop/login1/session/c1"
#define USER_PATH "/org/freedesktop/login1/user/0"
#define SEAT_IFACE "org.freedesktop.login1.Seat"
#define SESSION_IFACE "org.freedesktop.login1.Session"
#define USER_IFACE "org.freedesktop.login1.User"
#define MANAGER_IFACE "org.freedesktop.login1.Manager"

static int take_device_handler(sd_bus_message *m, void *userdata,
                               sd_bus_error *ret_error) {
        unsigned major = 0, minor = 0;
        char path[128], line[256], *devnode = NULL;
        FILE *uevent;
        int fd, r;

        r = sd_bus_message_read(m, "uu", &major, &minor);
        fprintf(stderr, "login1-stub: TakeDevice %u:%u (read r=%d)\n", major,
                minor, r);
        if (r < 0)
                return r;

        snprintf(path, sizeof(path), "/sys/dev/char/%u:%u/uevent", major, minor);
        uevent = fopen(path, "r");
        if (!uevent)
                return sd_bus_error_set_errnof(ret_error, ENOENT,
                                               "no uevent for %u:%u", major, minor);
        while (fgets(line, sizeof(line), uevent)) {
                if (strncmp(line, "DEVNAME=", 8) == 0) {
                        size_t n = strlen(line + 8);
                        while (n > 0 && (line[8 + n - 1] == '\n' || line[8 + n - 1] == '\r'))
                                n--;
                        devnode = strndup(line + 8, n);
                        break;
                }
        }
        fclose(uevent);
        if (!devnode)
                return sd_bus_error_set_errnof(ret_error, ENOENT,
                                               "no DEVNAME for %u:%u", major, minor);

        snprintf(path, sizeof(path), "/dev/%s", devnode);
        free(devnode);
        fd = open(path, O_RDWR | O_CLOEXEC | O_NONBLOCK);
        fprintf(stderr, "login1-stub: open(%s) = %d\n", path, fd);
        if (fd < 0)
                return -errno;

        {
                sd_bus_message *reply = NULL;
                r = sd_bus_message_new_method_return(m, &reply);
                fprintf(stderr, "login1-stub: new_method_return r=%d\n", r);
                if (r >= 0) {
                        r = sd_bus_message_append(reply, "hb", fd, 0);
                        fprintf(stderr, "login1-stub: append(hb) r=%d\n", r);
                }
                if (r >= 0) {
                        r = sd_bus_send(NULL, reply, NULL);
                        fprintf(stderr, "login1-stub: send r=%d (%s)\n", r,
                                r < 0 ? strerror(-r) : "ok");
                }
                sd_bus_message_unref(reply);
                close(fd);
        }
        return r < 0 ? r : 1; /* >0: method handled */
}

static int release_device_handler(sd_bus_message *m, void *userdata,
                                  sd_bus_error *ret_error) {
        unsigned major = 0, minor = 0;
        int r = sd_bus_message_read(m, "uu", &major, &minor);
        if (r < 0)
                return r;
        return sd_bus_reply_method_return(m, NULL);
}

static int take_control_handler(sd_bus_message *m, void *userdata,
                                sd_bus_error *ret_error) {
        int disabled = 0;
        int r = sd_bus_message_read(m, "b", &disabled);
        if (r < 0)
                return r;
        return sd_bus_reply_method_return(m, NULL);
}

static int no_args_handler(sd_bus_message *m, void *userdata,
                           sd_bus_error *ret_error) {
        return sd_bus_reply_method_return(m, NULL);
}

static int switch_to_handler(sd_bus_message *m, void *userdata,
                             sd_bus_error *ret_error) {
        unsigned n = 0;
        int r = sd_bus_message_read(m, "u", &n);
        if (r < 0)
                return r;
        return sd_bus_reply_method_return(m, NULL);
}

#define STUB_STR(v) ((const char *) (uintptr_t) (v))
#define STUB_OFF(v) ((uintptr_t) (v))

static int id_getter(sd_bus *bus, const char *path, const char *interface,
                     const char *property, sd_bus_message *reply,
                     void *userdata, sd_bus_error *ret_error) {
        return sd_bus_message_append(reply, "s", STUB_STR(userdata));
}

static int get_session_handler(sd_bus_message *m, void *userdata,
                               sd_bus_error *ret_error) {
        const char *id = NULL;
        int r = sd_bus_message_read(m, "s", &id);
        if (r < 0)
                return r;
        if (strcmp(id, "c1") != 0)
                return sd_bus_error_set_errnof(ret_error, ENOENT,
                                               "no session %s", id);
        return sd_bus_reply_method_return(m, "o", SESSION_PATH);
}

static int get_seat_handler(sd_bus_message *m, void *userdata,
                            sd_bus_error *ret_error) {
        const char *id = NULL;
        int r = sd_bus_message_read(m, "s", &id);
        if (r < 0)
                return r;
        if (strcmp(id, "seat0") != 0)
                return sd_bus_error_set_errnof(ret_error, ENOENT,
                                               "no seat %s", id);
        return sd_bus_reply_method_return(m, "o", SEAT_PATH);
}

/* Single-session stub: every pid belongs to session c1. */
static int get_session_by_pid_handler(sd_bus_message *m, void *userdata,
                                      sd_bus_error *ret_error) {
        unsigned pid = 0;
        int r = sd_bus_message_read(m, "u", &pid);
        if (r < 0)
                return r;
        return sd_bus_reply_method_return(m, "o", SESSION_PATH);
}

static int get_user_handler(sd_bus_message *m, void *userdata,
                            sd_bus_error *ret_error) {
        unsigned uid = 0;
        int r = sd_bus_message_read(m, "u", &uid);
        if (r < 0)
                return r;
        if (uid != 0)
                return sd_bus_error_set_errnof(ret_error, ENOENT,
                                               "no user %u", uid);
        return sd_bus_reply_method_return(m, "o", USER_PATH);
}

static int get_user_by_pid_handler(sd_bus_message *m, void *userdata,
                                   sd_bus_error *ret_error) {
        unsigned pid = 0;
        int r = sd_bus_message_read(m, "u", &pid);
        if (r < 0)
                return r;
        return sd_bus_reply_method_return(m, "o", USER_PATH);
}

static int type_getter(sd_bus *bus, const char *path, const char *interface,
                       const char *property, sd_bus_message *reply,
                       void *userdata, sd_bus_error *ret_error) {
        return sd_bus_message_append(reply, "s", "wayland");
}

static int state_getter(sd_bus *bus, const char *path, const char *interface,
                        const char *property, sd_bus_message *reply,
                        void *userdata, sd_bus_error *ret_error) {
        return sd_bus_message_append(reply, "s", "active");
}

static int class_getter(sd_bus *bus, const char *path, const char *interface,
                        const char *property, sd_bus_message *reply,
                        void *userdata, sd_bus_error *ret_error) {
        return sd_bus_message_append(reply, "s", "user");
}

static int seat_prop_getter(sd_bus *bus, const char *path,
                            const char *interface, const char *property,
                            sd_bus_message *reply, void *userdata,
                            sd_bus_error *ret_error) {
        return sd_bus_message_append(reply, "(so)", "seat0", SEAT_PATH);
}

/* Manager.Inhibit(who, why, what, mode) -> fd: hand back an fd the caller
 * can hold; there is no sleep inhibition to enforce. */
static int inhibit_handler(sd_bus_message *m, void *userdata,
                           sd_bus_error *ret_error) {
        const char *who = NULL, *why = NULL, *what = NULL, *mode = NULL;
        int r = sd_bus_message_read(m, "ssss", &who, &why, &what, &mode);
        if (r < 0)
                return r;

        int fd = open("/dev/null", O_RDWR | O_CLOEXEC);
        if (fd < 0)
                return -errno;

        {
                sd_bus_message *reply = NULL;
                r = sd_bus_message_new_method_return(m, &reply);
                if (r >= 0) {
                        r = sd_bus_message_append(reply, "h", fd);
                        if (r >= 0)
                                r = sd_bus_send(NULL, reply, NULL);
                }
                sd_bus_message_unref(reply);
                close(fd);
        }
        return r < 0 ? r : 1;
}

/* Manager.CanSuspend/CanReboot/... -> always "no" so callers skip the
 * corresponding actions. */
static int can_no_handler(sd_bus_message *m, void *userdata,
                          sd_bus_error *ret_error) {
        return sd_bus_reply_method_return(m, "s", "no");
}

static const char * const session_paths[] = { "c1", SESSION_PATH };

/* Generic array-of-(so) getter for the single session/seat we emulate. */
static int so_array_getter(sd_bus *bus, const char *path,
                           const char *interface, const char *property,
                           sd_bus_message *reply, void *userdata,
                           sd_bus_error *ret_error) {
        int r = sd_bus_message_open_container(reply, SD_BUS_TYPE_ARRAY, "(so)");
        if (r < 0)
                return r;
        r = sd_bus_message_append(reply, "(so)", session_paths[0],
                                  session_paths[1]);
        if (r < 0)
                return r;
        return sd_bus_message_close_container(reply);
}

/* Manager.Users -> a(uo) */
static int users_getter(sd_bus *bus, const char *path,
                        const char *interface, const char *property,
                        sd_bus_message *reply, void *userdata,
                        sd_bus_error *ret_error) {
        int r = sd_bus_message_open_container(reply, SD_BUS_TYPE_ARRAY, "(uo)");
        if (r < 0)
                return r;
        r = sd_bus_message_append(reply, "(uo)", (unsigned) 0, USER_PATH);
        if (r < 0)
                return r;
        return sd_bus_message_close_container(reply);
}

/* Session.User -> (uo) */
static int user_prop_getter(sd_bus *bus, const char *path,
                            const char *interface, const char *property,
                            sd_bus_message *reply, void *userdata,
                            sd_bus_error *ret_error) {
        return sd_bus_message_append(reply, "(uo)", (unsigned) 0, USER_PATH);
}

static int uid_getter(sd_bus *bus, const char *path, const char *interface,
                      const char *property, sd_bus_message *reply,
                      void *userdata, sd_bus_error *ret_error) {
        return sd_bus_message_append(reply, "u", (unsigned) 0);
}

static int name_getter(sd_bus *bus, const char *path, const char *interface,
                       const char *property, sd_bus_message *reply,
                       void *userdata, sd_bus_error *ret_error) {
        return sd_bus_message_append(reply, "s", "root");
}

static int runtime_path_getter(sd_bus *bus, const char *path,
                               const char *interface, const char *property,
                               sd_bus_message *reply, void *userdata,
                               sd_bus_error *ret_error) {
        return sd_bus_message_append(reply, "s", "/run/user/0");
}

static int false_getter(sd_bus *bus, const char *path, const char *interface,
                        const char *property, sd_bus_message *reply,
                        void *userdata, sd_bus_error *ret_error) {
        return sd_bus_message_append(reply, "b", 0);
}

static int true_getter(sd_bus *bus, const char *path, const char *interface,
                       const char *property, sd_bus_message *reply,
                       void *userdata, sd_bus_error *ret_error) {
        return sd_bus_message_append(reply, "b", 1);
}

static int timestamp_getter(sd_bus *bus, const char *path,
                            const char *interface, const char *property,
                            sd_bus_message *reply, void *userdata,
                            sd_bus_error *ret_error) {
        return sd_bus_message_append(reply, "t", (uint64_t) 0);
}

static const sd_bus_vtable session_vtable[] = {
        SD_BUS_VTABLE_START(0),
        SD_BUS_PROPERTY("Id", "s", id_getter, STUB_OFF("c1"),
                        SD_BUS_VTABLE_ABSOLUTE_OFFSET | SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("Active", "b", true_getter, 0, 0),
        SD_BUS_PROPERTY("State", "s", state_getter, 0, 0),
        SD_BUS_PROPERTY("Class", "s", class_getter, 0, 0),
        SD_BUS_PROPERTY("Type", "s", type_getter, 0, 0),
        SD_BUS_PROPERTY("Seat", "(so)", seat_prop_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("User", "(uo)", user_prop_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_METHOD("TakeDevice", "uu", "hb", take_device_handler, 0),
        SD_BUS_METHOD("ReleaseDevice", "uu", NULL, release_device_handler, 0),
        SD_BUS_METHOD("TakeControl", "b", NULL, take_control_handler, 0),
        SD_BUS_METHOD("ReleaseControl", NULL, NULL, no_args_handler, 0),
        SD_BUS_METHOD("SetType", "s", NULL, no_args_handler, 0),
        SD_BUS_VTABLE_END,
};

static const sd_bus_vtable seat_vtable[] = {
        SD_BUS_VTABLE_START(0),
        SD_BUS_PROPERTY("Id", "s", id_getter, STUB_OFF("seat0"),
                        SD_BUS_VTABLE_ABSOLUTE_OFFSET | SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("ActiveSession", "(so)", seat_prop_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("Sessions", "a(so)", so_array_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("CanGraphical", "b", true_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("CanTTY", "b", false_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_METHOD("SwitchTo", "u", NULL, switch_to_handler, 0),
        SD_BUS_VTABLE_END,
};

static const sd_bus_vtable user_vtable[] = {
        SD_BUS_VTABLE_START(0),
        SD_BUS_PROPERTY("UID", "u", uid_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("GID", "u", uid_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("Name", "s", name_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("Timestamp", "t", timestamp_getter, 0, 0),
        SD_BUS_PROPERTY("TimestampMonotonic", "t", timestamp_getter, 0, 0),
        SD_BUS_PROPERTY("RuntimePath", "s", runtime_path_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("Service", "s", id_getter, STUB_OFF("systemd"),
                        SD_BUS_VTABLE_ABSOLUTE_OFFSET | SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("Slice", "s", id_getter, STUB_OFF("user.slice"),
                        SD_BUS_VTABLE_ABSOLUTE_OFFSET | SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("State", "s", state_getter, 0, 0),
        SD_BUS_PROPERTY("Sessions", "a(so)", so_array_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("IdleHint", "b", false_getter, 0, 0),
        SD_BUS_PROPERTY("IdleSinceHint", "t", timestamp_getter, 0, 0),
        SD_BUS_PROPERTY("IdleSinceHintMonotonic", "t", timestamp_getter, 0, 0),
        SD_BUS_PROPERTY("Linger", "b", false_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_VTABLE_END,
};

static const sd_bus_vtable manager_vtable[] = {
        SD_BUS_VTABLE_START(0),
        SD_BUS_PROPERTY("Version", "s", id_getter, STUB_OFF("250"),
                        SD_BUS_VTABLE_ABSOLUTE_OFFSET | SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("Sessions", "a(so)", so_array_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("Seats", "a(so)", so_array_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("Users", "a(uo)", users_getter, 0,
                        SD_BUS_VTABLE_PROPERTY_CONST),
        SD_BUS_PROPERTY("IdleHint", "b", false_getter, 0, 0),
        SD_BUS_PROPERTY("Docked", "b", false_getter, 0, 0),
        SD_BUS_METHOD("Inhibit", "ssss", "h", inhibit_handler, 0),
        SD_BUS_METHOD("CanSuspend", NULL, "s", can_no_handler, 0),
        SD_BUS_METHOD("CanHibernate", NULL, "s", can_no_handler, 0),
        SD_BUS_METHOD("CanPowerOff", NULL, "s", can_no_handler, 0),
        SD_BUS_METHOD("CanReboot", NULL, "s", can_no_handler, 0),
        SD_BUS_METHOD("GetSession", "s", "o", get_session_handler, 0),
        SD_BUS_METHOD("GetSeat", "s", "o", get_seat_handler, 0),
        SD_BUS_METHOD("GetSessionByPID", "u", "o", get_session_by_pid_handler, 0),
        SD_BUS_METHOD("GetUser", "u", "o", get_user_handler, 0),
        SD_BUS_METHOD("GetUserByPID", "u", "o", get_user_by_pid_handler, 0),
        SD_BUS_VTABLE_END,
};

int main(void) {
        sd_bus *bus = NULL;
        sd_bus_slot *session_slot = NULL, *seat_slot = NULL;
        sd_bus_slot *user_slot = NULL, *manager_slot = NULL;
        int r;

        r = sd_bus_default_system(&bus);
        if (r < 0) {
                fprintf(stderr, "login1-stub: connect failed: %s\n", strerror(-r));
                return 1;
        }

        r = sd_bus_add_object_vtable(bus, &manager_slot, MANAGER_PATH,
                                     MANAGER_IFACE, manager_vtable, NULL);
        if (r >= 0)
                r = sd_bus_add_object_vtable(bus, &user_slot, USER_PATH,
                                             USER_IFACE, user_vtable, NULL);
        if (r >= 0)
                r = sd_bus_add_object_vtable(bus, &session_slot, SESSION_PATH,
                                             SESSION_IFACE, session_vtable,
                                             (void *) "c1");
        if (r >= 0)
                r = sd_bus_add_object_vtable(bus, &seat_slot, SEAT_PATH,
                                             SEAT_IFACE, seat_vtable, NULL);
        if (r >= 0)
                r = sd_bus_request_name(bus, BUS_NAME,
                                        SD_BUS_NAME_REPLACE_EXISTING);
        if (r < 0) {
                fprintf(stderr, "login1-stub: setup failed: %s\n", strerror(-r));
                return 1;
        }

        for (;;) {
                r = sd_bus_process(bus, NULL);
                if (r < 0) {
                        fprintf(stderr, "login1-stub: process failed: %s\n",
                                strerror(-r));
                        return 1;
                }
                if (r > 0)
                        continue;
                r = sd_bus_wait(bus, UINT64_MAX);
                if (r < 0) {
                        fprintf(stderr, "login1-stub: wait failed: %s\n",
                                strerror(-r));
                        return 1;
                }
        }
}
