/// Events that can be emitted by the BMI IMU
#[derive(Clone, Debug)]
pub enum Event {
    /// Accelerometer events measure the acceleration in a particular direction
    /// in units of meters per second squared (m/s^2).
    Accelerometer(AxisData, u64),
    /// Gyro events measure the angular velocity in radians per second.
    /// The u64 field is a timestamp in microseconds.
    Gyro(AxisData, u64),
}

/// AxisData represents the state of the accelerometer or gyro (x, y, z) values
#[derive(Clone, Debug, Default)]
pub struct AxisData {
    pub roll: f64,
    pub pitch: f64,
    pub yaw: f64,
}
