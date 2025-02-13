
namespace FCastReceiverService
{
    /// <summary>
    /// https://www.tizen.org/system
    /// </summary>
    public static class SystemInformation
    {
        /// <summary>
        /// The platform returns the build date. The build date is made when platform image is created
        /// </summary>
        public static string BuildDate { get; private set; }

        /// <summary>
        /// The platform returns a changelist number such as "tizen-mobile-RC2".
        /// The changelist number is made when platform image is created.
        /// </summary>
        public static string BuildId { get; private set; }

        /// <summary>
        /// The platform returns the build version information such as "20160307.1".
        /// The build version information is made when platform image is created.
        /// </summary>
        public static string BuildRelease { get; private set; }

        /// <summary>
        /// The platform returns the build information string.
        /// The build information string is made when platform image is created.
        /// </summary>
        public static string BuildString { get; private set; }

        /// <summary>
        /// The platform returns the build time. The build time is made when platform image is created.
        /// </summary>
        public static string BuildTime { get; private set; }

        /// <summary>
        /// The platform returns the build type such as "user" or "eng".
        /// The build type is made when platform image is created.
        /// </summary>
        public static string BuildType { get; private set; }

        /// <summary>
        /// The platform returns variant release information.
        /// The variant release information is made when platform image is created.
        /// </summary>
        public static string BuildVariant { get; private set; }

        /// <summary>
        /// The platform returns the manufacturer name.
        /// </summary>
        public static string Manufacturer { get; private set; }

        /// <summary>
        /// The platform returns the device model name.
        /// </summary>
        public static string ModelName { get; private set; }

        /// <summary>
        /// The platform returns the Platform name.
        /// </summary>
        public static string PlatformName { get; private set; }

        static SystemInformation()
        {
            string temp;

            if (Tizen.System.Information.TryGetValue("http://tizen.org/system/build.date", out temp) == false)
            {
                Serilog.Log.Warning($"Error initializing SystemInformation field: BuildDate");
            }
            BuildDate = temp;

            if (Tizen.System.Information.TryGetValue("http://tizen.org/system/build.id", out temp) == false)
            {
                Serilog.Log.Warning($"Error initializing SystemInformation field: BuildId");
            }
            BuildId = temp;

            if (Tizen.System.Information.TryGetValue("http://tizen.org/system/build.release", out temp) == false)
            {
                Serilog.Log.Warning($"Error initializing SystemInformation field: BuildRelease");
            }
            BuildRelease = temp;

            if (Tizen.System.Information.TryGetValue("http://tizen.org/system/build.string", out temp) == false)
            {
                Serilog.Log.Warning($"Error initializing SystemInformation field: BuildString");
            }
            BuildString = temp;

            if (Tizen.System.Information.TryGetValue("http://tizen.org/system/build.time", out temp) == false)
            {
                Serilog.Log.Warning($"Error initializing SystemInformation field: BuildTime");
            }
            BuildTime = temp;

            if (Tizen.System.Information.TryGetValue("http://tizen.org/system/build.type", out temp) == false)
            {
                Serilog.Log.Warning($"Error initializing SystemInformation field: BuildType");
            }
            BuildType = temp;

            if (Tizen.System.Information.TryGetValue("http://tizen.org/system/build.variant", out temp) == false)
            {
                Serilog.Log.Warning($"Error initializing SystemInformation field: BuildVariant");
            }
            BuildVariant = temp;

            if (Tizen.System.Information.TryGetValue("http://tizen.org/system/manufacturer", out temp) == false)
            {
                Serilog.Log.Warning($"Error initializing SystemInformation field: Manufacturer");
            }
            Manufacturer = temp;

            if (Tizen.System.Information.TryGetValue("http://tizen.org/system/model_name", out temp) == false)
            {
                Serilog.Log.Warning($"Error initializing SystemInformation field: ModelName");
            }
            ModelName = temp;

            if (Tizen.System.Information.TryGetValue("http://tizen.org/system/platform.name", out temp) == false)
            {
                Serilog.Log.Warning($"Error initializing SystemInformation field: PlatformName");
            }
            PlatformName = temp;
        }
    }
}
