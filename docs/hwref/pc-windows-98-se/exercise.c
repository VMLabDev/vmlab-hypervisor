#include <windows.h>
#include <stdio.h>
#include <string.h>

int main(void)
{
    DEVMODE mode;
    DEVMODE original;
    FILE *file;
    char data[64];
    char path[MAX_PATH];
    char root[4] = "D:\\";
    int drive;
    int index;
    DWORD written;
    HANDLE disk;
    HDC screen;
    HDC buffer;
    HBITMAP bitmap;
    HGDIOBJ old;
    RECT rectangle;
    memset(&original, 0, sizeof(original));
    original.dmSize = sizeof(original);
    printf("current_mode=%d\n", EnumDisplaySettings(NULL, ENUM_CURRENT_SETTINGS, &original));
    printf("dimensions=%lux%lu depth=%lu\n", original.dmPelsWidth, original.dmPelsHeight, original.dmBitsPerPel);
    for (index = 0; index < 2; index++) {
        mode = original;
        mode.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT | DM_BITSPERPEL;
        mode.dmPelsWidth = index ? 800 : 640;
        mode.dmPelsHeight = index ? 600 : 480;
        mode.dmBitsPerPel = 16;
        printf("mode_%lux%lu=%ld\n", mode.dmPelsWidth, mode.dmPelsHeight, ChangeDisplaySettings(&mode, 0));
        screen = GetDC(NULL);
        buffer = CreateCompatibleDC(screen);
        bitmap = CreateCompatibleBitmap(screen, 240, 160);
        old = SelectObject(buffer, bitmap);
        rectangle.left = rectangle.top = 0;
        rectangle.right = 240;
        rectangle.bottom = 160;
        FillRect(buffer, &rectangle, (HBRUSH)GetStockObject(WHITE_BRUSH));
        TextOut(buffer, 5, 5, "VMLAB HWREF", 11);
        printf("bitblt=%d\n", BitBlt(screen, 10, 10, 240, 160, buffer, 0, 0, SRCCOPY));
        printf("screen_copy=%d\n", BitBlt(screen, 30, 30, 200, 100, screen, 10, 10, SRCCOPY));
        SelectObject(buffer, old);
        DeleteObject(bitmap);
        DeleteDC(buffer);
        ReleaseDC(NULL, screen);
        Sleep(1000);
    }
    printf("restore_mode=%ld\n", ChangeDisplaySettings(&original, 0));
    GetTempPath(sizeof(path), path);
    strcat(path, "hwref.tmp");
    disk = CreateFile(path, GENERIC_WRITE | GENERIC_READ, 0, NULL, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL | FILE_FLAG_WRITE_THROUGH, NULL);
    if (disk != INVALID_HANDLE_VALUE) {
        printf("disk_write=%d\n", WriteFile(disk, "HWREF DISK CHECK", 16, &written, NULL));
        printf("disk_flush=%d\n", FlushFileBuffers(disk));
        SetFilePointer(disk, 0, NULL, FILE_BEGIN);
        memset(data, 0, sizeof(data));
        printf("disk_read=%d\n", ReadFile(disk, data, 16, &written, NULL));
        printf("disk_content=%s\n", data);
        CloseHandle(disk);
        DeleteFile(path);
    } else {
        printf("disk_open_error=%lu\n", GetLastError());
    }
    for (drive = 'D'; drive <= 'Z'; drive++) {
        root[0] = drive;
        if (GetDriveType(root) == DRIVE_CDROM) {
            sprintf(path, "%c:\\HWREF.TXT", drive);
            file = fopen(path, "r");
            printf("cd_%c=%s\n", drive, file ? "opened" : "failed");
            if (file) {
                if (fgets(data, sizeof(data), file)) printf("cd_content=%s", data);
                fclose(file);
            }
        }
    }
    return 0;
}
