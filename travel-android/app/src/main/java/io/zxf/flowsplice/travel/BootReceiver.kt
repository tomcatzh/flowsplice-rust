package io.zxf.flowsplice.travel

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == Intent.ACTION_BOOT_COMPLETED &&
            TravelService.shouldAutoStart(context) &&
            TravelInstallation.isInstalled(context)
        ) {
            TravelRepository.start(context)
        }
    }
}
