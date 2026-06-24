#pragma once

#include <stdint.h>

typedef uint64_t pthread_t;
typedef struct { int _unused; } pthread_attr_t;
typedef struct { int _unused; } pthread_mutexattr_t;
typedef struct { int _unused; } pthread_condattr_t;

#ifndef ENOMEM
#define ENOMEM    12
#endif
#ifndef EBUSY
#define EBUSY     16
#endif
#ifndef EINVAL
#define EINVAL    22
#endif
#ifndef ETIMEDOUT
#define ETIMEDOUT 110
#endif

struct timespec {
    int64_t tv_sec;
    int64_t tv_nsec;
};

typedef struct {
    uint64_t fd;
    int      init;
} pthread_mutex_t;

#define PTHREAD_MUTEX_INITIALIZER { (uint64_t)-1, 0 }

typedef struct {
    uint64_t sem;
    uint64_t waiters_lock;
    int64_t  waiters;
    int      init;
} pthread_cond_t;

#define PTHREAD_COND_INITIALIZER { (uint64_t)-1, (uint64_t)-1, 0, 0 }

int pthread_mutex_init(pthread_mutex_t *mutex, const pthread_mutexattr_t *attr);
int pthread_mutex_destroy(pthread_mutex_t *mutex);
int pthread_mutex_lock(pthread_mutex_t *mutex);
int pthread_mutex_unlock(pthread_mutex_t *mutex);
int pthread_mutex_trylock(pthread_mutex_t *mutex);

int pthread_cond_init(pthread_cond_t *cond, const pthread_condattr_t *attr);
int pthread_cond_destroy(pthread_cond_t *cond);
int pthread_cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex);
int pthread_cond_timedwait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                           const struct timespec *abstime);
int pthread_cond_signal(pthread_cond_t *cond);
int pthread_cond_broadcast(pthread_cond_t *cond);

int pthread_attr_init(pthread_attr_t *attr);
int pthread_attr_destroy(pthread_attr_t *attr);
int pthread_create(pthread_t *thread, const pthread_attr_t *attr, void *(*start_routine)(void *), void *arg);
int pthread_join(pthread_t thread, void **retval);
int pthread_detach(pthread_t thread);
void pthread_exit(void *retval);
pthread_t pthread_self(void);
int pthread_equal(pthread_t t1, pthread_t t2);
