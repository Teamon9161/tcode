#include <fcntl.h>
#include <linux/input.h>
#include <linux/uinput.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

static void emit(int fd, int type, int code, int value) {
  struct input_event event;
  memset(&event, 0, sizeof(event));
  event.type = type;
  event.code = code;
  event.value = value;
  if (write(fd, &event, sizeof(event)) != sizeof(event)) {
    perror("write input_event");
    exit(1);
  }
}

static void sync_events(int fd) { emit(fd, EV_SYN, SYN_REPORT, 0); }

static void pause_ms(long millis) {
  struct timespec delay = {
      .tv_sec = millis / 1000,
      .tv_nsec = (millis % 1000) * 1000000,
  };
  nanosleep(&delay, NULL);
}

int main(int argc, char **argv) {
  if (argc != 3) {
    fprintf(stderr, "usage: %s SCREEN_X SCREEN_Y\n", argv[0]);
    return 2;
  }
  const int target_x = atoi(argv[1]);
  const int target_y = atoi(argv[2]);
  const int fd = open("/dev/uinput", O_WRONLY | O_NONBLOCK);
  if (fd < 0) {
    perror("open /dev/uinput");
    return 1;
  }

  if (ioctl(fd, UI_SET_EVBIT, EV_KEY) < 0 ||
      ioctl(fd, UI_SET_KEYBIT, BTN_LEFT) < 0 ||
      ioctl(fd, UI_SET_EVBIT, EV_REL) < 0 ||
      ioctl(fd, UI_SET_RELBIT, REL_X) < 0 ||
      ioctl(fd, UI_SET_RELBIT, REL_Y) < 0) {
    perror("uinput capability ioctl");
    return 1;
  }

  struct uinput_setup setup;
  memset(&setup, 0, sizeof(setup));
  snprintf(setup.name, UINPUT_MAX_NAME_SIZE, "tcode compositor probe mouse");
  setup.id.bustype = BUS_USB;
  setup.id.vendor = 0x1234;
  setup.id.product = 0x5679;
  setup.id.version = 1;
  if (ioctl(fd, UI_DEV_SETUP, &setup) < 0 || ioctl(fd, UI_DEV_CREATE) < 0) {
    perror("create uinput device");
    return 1;
  }

  // Give the compositor time to discover the temporary device, then clamp at
  // the desktop origin and walk in one-pixel steps to avoid pointer acceleration.
  pause_ms(3000);
  emit(fd, EV_REL, REL_X, -10000);
  emit(fd, EV_REL, REL_Y, -10000);
  sync_events(fd);
  pause_ms(150);

  int x = 0;
  int y = 0;
  while (x < target_x || y < target_y) {
    if (x < target_x) {
      emit(fd, EV_REL, REL_X, 1);
      x += 1;
    }
    if (y < target_y) {
      emit(fd, EV_REL, REL_Y, 1);
      y += 1;
    }
    sync_events(fd);
    pause_ms(1);
  }
  pause_ms(250);
  emit(fd, EV_KEY, BTN_LEFT, 1);
  sync_events(fd);
  emit(fd, EV_KEY, BTN_LEFT, 0);
  sync_events(fd);
  pause_ms(250);

  ioctl(fd, UI_DEV_DESTROY);
  close(fd);
  return 0;
}
