#include <dos.h>
#include <conio.h>
#include <stdio.h>
#include <string.h>

int main(void)
{
    union REGS in;
    union REGS out;
    FILE *file;
    char text[80];
    memset(&in, 0, sizeof(in));
    in.x.ax = 0x4f02;
    in.x.bx = 0x101;
    int86(0x10, &in, &out);
    delay(1000);
    in.x.ax = 3;
    int86(0x10, &in, &out);
    printf("VBE 640x480x8 requested; text mode 3 restored\n");
    file = fopen("C:\\HWREF.TMP", "w");
    if (file) {
        fputs("HWREF DOS DISK CHECK", file);
        printf("disk_close=%d\n", fclose(file));
        file = fopen("C:\\HWREF.TMP", "r");
        if (file) {
            if (fgets(text, sizeof(text), file)) printf("disk_content=%s\n", text);
            fclose(file);
        }
        remove("C:\\HWREF.TMP");
    }
    file = fopen("D:\\HWREF.TXT", "r");
    printf("cd_open=%s\n", file ? "ok" : "failed");
    if (file) {
        if (fgets(text, sizeof(text), file)) printf("cd_content=%s", text);
        fclose(file);
    }
    if (kbhit()) printf("keyboard_byte=%d\n", getch());
    in.x.ax = 3;
    int86(0x33, &in, &out);
    printf("mouse_x=%u mouse_y=%u buttons=%u\n", out.x.cx, out.x.dx, out.x.bx);
    return 0;
}
