use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[async_metronome::test]
async fn test_ordering() {
    let ai1 = Arc::new(AtomicUsize::new(0));
    let ai2 = ai1.clone();
    let ai3 = ai1.clone();

    let ordering = Ordering::SeqCst;

    let task1 = async move {
        ai1.compare_exchange(0, 1, ordering, ordering).unwrap();
        async_metronome::await_tick!(3);
        assert_eq!(ai1.load(ordering), 3);
    };

    let task2 = async move {
        async_metronome::await_tick!(1);
        assert_eq!(ai2.compare_exchange(1, 2, ordering, ordering).unwrap(), 1);
        async_metronome::await_tick!(3);
        assert_eq!(ai2.load(ordering), 3);
    };

    let task3 = async move {
        async_metronome::await_tick!(2);
        assert_eq!(ai3.compare_exchange(2, 3, ordering, ordering).unwrap(), 2);
    };

    let s1 = async_metronome::spawn(task1);
    let s2 = async_metronome::spawn(task2);
    let s3 = async_metronome::spawn(task3);

    s1.await;
    s2.await;
    s3.await;
}
