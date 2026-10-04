// A plugin's C# sources (platforms/windows/**/*.cs) are compiled into the app, the way a plugin's
// platforms/android Java/Kotlin and platforms/ios Objective-C/Swift sources are.
namespace TestPlugin.Native
{
    public class PluginGreeter
    {
        public string Greet(string name) => $"Hello from the plugin, {name}!";

        public static string PluginName => "test-plugin";
    }
}
