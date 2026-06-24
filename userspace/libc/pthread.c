#include <pthread.h>
#include <semaphore.h>
#include <sys/syscall.h>

/* Retries sys_wait on signal interruption. Used wherever POSIX forbids EINTR. */
static syscall_status_t wait_uninterruptible(uint64_t fd) {
    syscall_status_t r;
    do { r = sys_wait(fd, -1); } while (r == E_WAIT_INTERRUPTED);
    return r;
}

static int mutex_ensure_init(pthread_mutex_t *m) {
    if (__atomic_load_n(&m->init, __ATOMIC_ACQUIRE))
        return 0;

    syscall_status_t fd = sys_create_sync_object(SYNC_SEMAPHORE, 1, 1, 0);
    if (fd < 0)
        return ENOMEM;

    uint64_t candidate = (uint64_t)fd;
    uint64_t expected  = (uint64_t)-1;
    int won = __atomic_compare_exchange_n(
        &m->fd, &expected, candidate,
        0, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE
    );
    if (!won)
        sys_close(candidate);

    __atomic_store_n(&m->init, 1, __ATOMIC_RELEASE);
    return 0;
}

int pthread_mutex_init(pthread_mutex_t *mutex, const pthread_mutexattr_t *attr) {
    (void)attr;
    syscall_status_t fd = sys_create_sync_object(SYNC_SEMAPHORE, 1, 1, 0);
    if (fd < 0)
        return ENOMEM;
    mutex->fd   = (uint64_t)fd;
    mutex->init = 1;
    return 0;
}

int pthread_mutex_destroy(pthread_mutex_t *mutex) {
    if (!mutex->init)
        return EINVAL;
    sys_close(mutex->fd);
    mutex->fd   = (uint64_t)-1;
    mutex->init = 0;
    return 0;
}

int pthread_mutex_lock(pthread_mutex_t *mutex) {
    int r = mutex_ensure_init(mutex);
    if (r) return r;
    return wait_uninterruptible(mutex->fd) == E_SUCCESS ? 0 : EINVAL;
}

int pthread_mutex_unlock(pthread_mutex_t *mutex) {
    int r = mutex_ensure_init(mutex);
    if (r) return r;
    return sys_signal(mutex->fd) == E_SUCCESS ? 0 : EINVAL;
}

int pthread_mutex_trylock(pthread_mutex_t *mutex) {
    int r = mutex_ensure_init(mutex);
    if (r) return r;
    syscall_status_t res = sys_wait(mutex->fd, 0);
    if (res == E_SUCCESS) return 0;
    if (res == E_TIMEOUT) return EBUSY;
    return EINVAL;
}

static int cond_ensure_init(pthread_cond_t *c) {
    if (__atomic_load_n(&c->init, __ATOMIC_ACQUIRE))
        return 0;

    syscall_status_t sem = sys_create_sync_object(SYNC_SEMAPHORE, 0, 0x7FFFFFFF, 0);
    if (sem < 0)
        return ENOMEM;

    syscall_status_t wlock = sys_create_sync_object(SYNC_SEMAPHORE, 1, 1, 0);
    if (wlock < 0) {
        sys_close((uint64_t)sem);
        return ENOMEM;
    }

    uint64_t candidate = (uint64_t)sem;
    uint64_t expected  = (uint64_t)-1;
    int won = __atomic_compare_exchange_n(
        &c->sem, &expected, candidate,
        0, __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE
    );

    if (won) {
        c->waiters_lock = (uint64_t)wlock;
        __atomic_store_n(&c->init, 1, __ATOMIC_RELEASE);
    } else {
        sys_close((uint64_t)sem);
        sys_close((uint64_t)wlock);
        /* Spin until the winning thread publishes init = 1. */
        while (!__atomic_load_n(&c->init, __ATOMIC_ACQUIRE));
    }
    return 0;
}

int pthread_cond_init(pthread_cond_t *cond, const pthread_condattr_t *attr) {
    (void)attr;
    syscall_status_t sem = sys_create_sync_object(SYNC_SEMAPHORE, 0, 0x7FFFFFFF, 0);
    if (sem < 0)
        return ENOMEM;
    syscall_status_t wlock = sys_create_sync_object(SYNC_SEMAPHORE, 1, 1, 0);
    if (wlock < 0) {
        sys_close((uint64_t)sem);
        return ENOMEM;
    }
    cond->sem          = (uint64_t)sem;
    cond->waiters_lock = (uint64_t)wlock;
    cond->waiters      = 0;
    cond->init         = 1;
    return 0;
}

int pthread_cond_destroy(pthread_cond_t *cond) {
    if (!cond->init)
        return EINVAL;
    sys_close(cond->sem);
    sys_close(cond->waiters_lock);
    cond->sem          = (uint64_t)-1;
    cond->waiters_lock = (uint64_t)-1;
    cond->waiters      = 0;
    cond->init         = 0;
    return 0;
}

int pthread_cond_wait(pthread_cond_t *cond, pthread_mutex_t *mutex) {
    int r = cond_ensure_init(cond);
    if (r) return r;

    /* Increment waiters while still holding mutex so signal/broadcast
     * issued after we drop the mutex but before we sleep is not lost. */
    wait_uninterruptible(cond->waiters_lock);
    cond->waiters++;
    sys_signal(cond->waiters_lock);

    pthread_mutex_unlock(mutex);
    wait_uninterruptible(cond->sem);
    pthread_mutex_lock(mutex);
    return 0;
}

int pthread_cond_timedwait(pthread_cond_t *cond, pthread_mutex_t *mutex,
                           const struct timespec *abstime) {
    int r = cond_ensure_init(cond);
    if (r) return r;

    uint64_t deadline_ms = (uint64_t)abstime->tv_sec * 1000
                         + (uint64_t)abstime->tv_nsec / 1000000;

    wait_uninterruptible(cond->waiters_lock);
    cond->waiters++;
    sys_signal(cond->waiters_lock);

    pthread_mutex_unlock(mutex);

    syscall_status_t res = E_TIMEOUT;
    uint64_t now_ms;
    while (1) {
        if (sys_get_time_ms(CLOCK_MONOTONIC, &now_ms) != E_SUCCESS) break;
        if (now_ms >= deadline_ms) { res = E_TIMEOUT; break; }
        res = sys_wait(cond->sem, (ssize_t)(deadline_ms - now_ms));
        if (res != E_WAIT_INTERRUPTED) break;
    }

    if (res != E_SUCCESS) {
        wait_uninterruptible(cond->waiters_lock);
        if (cond->waiters > 0)
            cond->waiters--;
        sys_signal(cond->waiters_lock);
    }

    pthread_mutex_lock(mutex);
    return res == E_TIMEOUT ? ETIMEDOUT : (res == E_SUCCESS ? 0 : EINVAL);
}

int pthread_cond_signal(pthread_cond_t *cond) {
    int r = cond_ensure_init(cond);
    if (r) return r;

    wait_uninterruptible(cond->waiters_lock);
    if (cond->waiters > 0) {
        cond->waiters--;
        sys_signal(cond->sem);
    }
    sys_signal(cond->waiters_lock);
    return 0;
}

int pthread_cond_broadcast(pthread_cond_t *cond) {
    int r = cond_ensure_init(cond);
    if (r) return r;

    wait_uninterruptible(cond->waiters_lock);
    int64_t count = cond->waiters;
    cond->waiters = 0;
    sys_signal(cond->waiters_lock);

    /* Signal outside the lock so woken threads can re-enter cond_wait
     * without deadlocking on waiters_lock. */
    for (int64_t i = 0; i < count; i++)
        sys_signal(cond->sem);
    return 0;
}

int sem_init(sem_t *sem, int pshared, unsigned int value) {
    (void)pshared;
    syscall_status_t fd = sys_create_sync_object(SYNC_SEMAPHORE, (uint64_t)value, 0x7FFFFFFF, 0);
    if (fd < 0)
        return -1;
    sem->fd   = (uint64_t)fd;
    sem->init = 1;
    return 0;
}

int sem_destroy(sem_t *sem) {
    if (!sem->init)
        return -1;
    sys_close(sem->fd);
    sem->fd   = (uint64_t)-1;
    sem->init = 0;
    return 0;
}

int sem_wait(sem_t *sem) {
    if (!sem->init)
        return -1;
    /* POSIX specifies sem_wait returns EINTR on interruption; callers handle it. */
    return sys_wait(sem->fd, -1) == E_SUCCESS ? 0 : -1;
}

int sem_post(sem_t *sem) {
    if (!sem->init)
        return -1;
    return sys_signal(sem->fd) == E_SUCCESS ? 0 : -1;
}

int sem_trywait(sem_t *sem) {
    if (!sem->init)
        return -1;
    syscall_status_t res = sys_wait(sem->fd, 0);
    return res == E_SUCCESS ? 0 : -1;
}
