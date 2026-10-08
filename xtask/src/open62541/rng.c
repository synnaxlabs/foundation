/* Prints the values that start value 1 gives on this thread after another thread drew
 * from start value 2, then the values that start value 1 gives on a thread alone. With
 * one random state per thread, the two lists are equal. */

#include <open62541/types.h>
#include <open62541/util.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>

/* The copy calls no clock here, so a call is a defect. */
UA_DateTime UA_DateTime_now(void) { abort(); }
UA_DateTime UA_DateTime_nowMonotonic(void) { abort(); }
UA_Int64 UA_DateTime_localTimeUtcOffset(void) { abort(); }

enum { DRAWS = 8 };

struct draws {
    UA_UInt64 start;
    UA_UInt32 values[DRAWS];
};

static void *start_and_draw(void *arg) {
    struct draws *draws = arg;
    UA_random_seed_deterministic(draws->start);
    for (int i = 0; i < DRAWS; i++)
        draws->values[i] = UA_UInt32_random();
    return NULL;
}

/* Runs `start_and_draw` on a new thread, and returns when that thread ends. */
static void on_thread(struct draws *draws) {
    pthread_t thread;
    if (pthread_create(&thread, NULL, start_and_draw, draws) != 0 ||
        pthread_join(thread, NULL) != 0)
        abort();
}

static void print(const char *label, const struct draws *draws) {
    printf("%s:", label);
    for (int i = 0; i < DRAWS; i++)
        printf(" %u", (unsigned)draws->values[i]);
    printf("\n");
}

int main(void) {
    struct draws after = {.start = 1}, other = {.start = 2}, alone = {.start = 1};
    UA_random_seed_deterministic(after.start);
    on_thread(&other);
    for (int i = 0; i < DRAWS; i++)
        after.values[i] = UA_UInt32_random();
    on_thread(&alone);
    print("after another thread", &after);
    print("alone", &alone);
    return 0;
}
