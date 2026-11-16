// Minimal UPLAY R1 client: loads a game's uplay_r1_loader64.dll and drives the
// startup sequence a game performs, printing every result. Used to observe how
// the SDK locates and talks to Connect, without running a real game.
//
//   harness.exe <path\to\uplay_r1_loader64.dll> <uplay_id> [seconds]
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

typedef int (__cdecl *start_fn)(unsigned, unsigned);
typedef int (__cdecl *int_fn)(void);
typedef const char *(__cdecl *str_fn)(void);
typedef int (__cdecl *owned_fn)(unsigned);
typedef int (__cdecl *event_fn)(void *);

/* R1 SDK structs (layout as used by games; oversized for safety). */
typedef struct { void *reserved; unsigned completed; unsigned result; char pad[64]; } UPLAY_Overlapped;
typedef struct { unsigned slot_id; const char *name; unsigned size; } UPLAY_SAVE_Game;
typedef struct { unsigned count; UPLAY_SAVE_Game **games; } UPLAY_SAVE_GameList;
typedef int (__cdecl *get_saves_fn)(UPLAY_SAVE_GameList **, UPLAY_Overlapped *);
typedef int (__cdecl *has_completed_fn)(UPLAY_Overlapped *);
typedef int (__cdecl *op_result_fn)(UPLAY_Overlapped *, int *);

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc < 3) { fprintf(stderr, "usage: %s loader.dll uplay_id [seconds]\n", argv[0]); return 2; }
    unsigned id = strtoul(argv[2], NULL, 10);
    int seconds = argc > 3 ? atoi(argv[3]) : 20;
    HMODULE h = LoadLibraryA(argv[1]);
    if (!h) { printf("LoadLibrary failed: %lu\n", GetLastError()); return 1; }
#define SYM(t, n) t n = (t)GetProcAddress(h, "UPLAY_" #n)
    SYM(start_fn, Start); SYM(int_fn, Update); SYM(int_fn, Quit); int_fn GetLastError_ = (int_fn)GetProcAddress(h, "UPLAY_GetLastError");
    SYM(str_fn, USER_GetNameUtf8); SYM(str_fn, USER_GetAccountIdUtf8); SYM(str_fn, USER_GetTicketUtf8);
    SYM(owned_fn, USER_IsOwned); SYM(int_fn, USER_IsInOfflineMode); SYM(event_fn, GetNextEvent);
    printf("UPLAY_Start(%u, 0) = %d\n", id, Start(id, 0));
    printf("UPLAY_GetLastError = %d\n", GetLastError_());
    for (int i = 0; i < seconds * 10; i++) {
        int u = Update();
        if (i % 10 == 0) printf("t=%ds UPLAY_Update = %d\n", i / 10, u);
        Sleep(100);
    }
    get_saves_fn GetSaves = (get_saves_fn)GetProcAddress(h, "UPLAY_SAVE_GetSavegames");
    has_completed_fn HasCompleted = (has_completed_fn)GetProcAddress(h, "UPLAY_HasOverlappedOperationCompleted");
    op_result_fn OpResult = (op_result_fn)GetProcAddress(h, "UPLAY_GetOverlappedOperationResult");
    if (getenv("HARNESS_SAVES")) {
        UPLAY_SAVE_GameList *list = NULL;
        UPLAY_Overlapped ov = {0};
        printf("UPLAY_SAVE_GetSavegames = %d\n", GetSaves(&list, &ov));
        for (int i = 0; i < 50 && !HasCompleted(&ov); i++) { Update(); Sleep(100); }
        int res = -1;
        printf("completed=%d result_call=%d result=%d\n", HasCompleted(&ov), OpResult(&ov, &res), res);
        if (list) {
            printf("save count=%u\n", list->count);
            for (unsigned i = 0; i < list->count && i < 32; i++)
                printf("  slot %u size %u name '%s'\n", list->games[i]->slot_id, list->games[i]->size,
                       list->games[i]->name ? list->games[i]->name : "(null)");
        }
    }
    const char *s;
    printf("name=%s\n", (s = USER_GetNameUtf8()) ? s : "(null)");
    printf("account=%s\n", (s = USER_GetAccountIdUtf8()) ? s : "(null)");
    printf("ticket=%s\n", (s = USER_GetTicketUtf8()) ? "(present)" : "(null)");
    printf("IsOwned(%u)=%d offline=%d\n", id, USER_IsOwned(id), USER_IsInOfflineMode());
    printf("UPLAY_Quit = %d\n", Quit());
    return 0;
}
