package app.airhand.android.wheelpatch

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Slider
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.dp
import app.airhand.android.R
import kotlin.math.roundToInt

@Composable
fun TabletWheelSettings() {
    val context = LocalContext.current
    val prefs = remember(context) { TabletWheelCompat.preferences(context) }
    var enabled by remember { mutableStateOf(prefs.getBoolean("enabled", TabletWheelCompat.defaultEnabled())) }
    var notch by remember { mutableFloatStateOf(prefs.getInt("notchDp", 48).toFloat()) }
    var diagnostics by remember { mutableStateOf(prefs.getBoolean("diagnostics", false)) }
    Column(Modifier.fillMaxWidth().padding(horizontal = 24.dp, vertical = 8.dp)) {
        Text(stringResource(R.string.wheel_patch_title), style = MaterialTheme.typography.titleSmall)
        Switch(checked = enabled, onCheckedChange = {
            enabled = it
            prefs.edit().putBoolean("enabled", it).apply()
        })
        Text(stringResource(R.string.wheel_patch_description), style = MaterialTheme.typography.bodySmall)
        if (enabled) {
            Text(stringResource(R.string.wheel_patch_scale, notch.roundToInt()))
            Slider(value = notch, onValueChange = { notch = it }, valueRange = 12f..120f,
                onValueChangeFinished = { prefs.edit().putInt("notchDp", notch.roundToInt()).apply() })
        }
        Text(
            stringResource(if (diagnostics) R.string.wheel_patch_diagnostics_on else R.string.wheel_patch_diagnostics_off),
            style = MaterialTheme.typography.bodySmall,
            modifier = Modifier.clickable {
                diagnostics = !diagnostics
                prefs.edit().putBoolean("diagnostics", diagnostics).apply()
            }.padding(vertical = 8.dp),
        )
    }
}
