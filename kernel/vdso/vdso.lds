LINUX_2.6 {
    global: clock_gettime; gettimeofday; time;
            __vdso_clock_gettime; __vdso_gettimeofday; __vdso_time;
    local: *;
};
